//! CMake adapter: configure with `cmake -S/-B`, build with `cmake --build`.
//!
//! Invocation (verified against CMake 4.x; `-S`/`-B` need 3.13+):
//! ```text
//! cmake -S <src> -B <build> -DCMAKE_BUILD_TYPE=<type>
//!       -DCMAKE_INSTALL_PREFIX=<build>/install
//!       [-DCMAKE_CXX_COMPILER=<tc.cxx> [-DCMAKE_C_COMPILER=<sibling>]]
//!       [-Dkey=value ...]
//! cmake --build <build> --config <type> [--target t ...] [--parallel N]
//! ```
//! `--config` is required by multi-config generators (Visual Studio is the
//! Windows default) and ignored by single-config ones. One `--target` flag
//! is emitted per requested target (verified: builds every named target).
//! Only the `cmake` binary is ever executed — nothing else from the foreign
//! source runs at probe time.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use rivet_manifest::AdapterSpec;

use super::discover::{discover_include_dirs, discover_libs, lib_dirs_of};
use super::util::{require_tool, run_capture};
use super::{AdapterCtx, AdapterOutput, Error};

/// Configure + build `source_dir` per `spec`, discovering outputs under
/// `ctx.build_dir`.
pub fn build(
    source_dir: &Path,
    spec: &AdapterSpec,
    ctx: &AdapterCtx,
) -> Result<AdapterOutput, Error> {
    if ctx.build_type.trim().is_empty() {
        return Err(Error::InvalidSpec {
            message: "adapter build type is empty. Fix: use `Debug` or `Release`".to_string(),
        });
    }
    let cmake = require_tool("cmake")?;
    std::fs::create_dir_all(&ctx.build_dir)?;

    let configure_what = format!("configure {} with cmake", source_dir.display());
    let mut configure = Command::new(&cmake);
    configure.args(configure_args(source_dir, spec, ctx));
    if let Some(tc) = &ctx.toolchain {
        tc.apply_env(&mut configure);
    }
    run_capture(
        &mut configure,
        &configure_what,
        "inspect the foreign CMakeLists.txt (missing `project()`/`cmake_minimum_required`, unknown `-D` option, or no compiler found) and retry",
        &[],
    )?;

    let mut build_cmd = Command::new(&cmake);
    build_cmd.args(build_args(spec, ctx));
    if let Some(tc) = &ctx.toolchain {
        tc.apply_env(&mut build_cmd);
    }
    run_capture(
        &mut build_cmd,
        &format!("build {} with cmake", ctx.build_dir.display()),
        "read the compiler errors above — they come from the foreign project's own sources",
        &[],
    )?;

    let libs = discover_libs(&ctx.build_dir, &spec.targets)?;
    let lib_dirs = lib_dirs_of(&libs);
    let include_dirs = discover_include_dirs(source_dir, &ctx.build_dir);
    Ok(AdapterOutput {
        include_dirs,
        lib_dirs,
        libs,
        defines: Vec::new(),
    })
}

/// `cmake -S src -B build -DCMAKE_BUILD_TYPE=… -DCMAKE_INSTALL_PREFIX=…`
/// plus compiler forwarding and spec options as `-D`.
pub(crate) fn configure_args(
    source_dir: &Path,
    spec: &AdapterSpec,
    ctx: &AdapterCtx,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-S"),
        source_dir.as_os_str().to_os_string(),
        OsString::from("-B"),
        ctx.build_dir.as_os_str().to_os_string(),
        OsString::from(format!("-DCMAKE_BUILD_TYPE={}", ctx.build_type)),
        OsString::from(format!(
            "-DCMAKE_INSTALL_PREFIX={}",
            ctx.build_dir.join("install").display()
        )),
    ];
    if let Some(tc) = &ctx.toolchain {
        if !spec.options.contains_key("CMAKE_CXX_COMPILER") {
            args.push(OsString::from(format!(
                "-DCMAKE_CXX_COMPILER={}",
                tc.cxx.display()
            )));
            if !spec.options.contains_key("CMAKE_C_COMPILER") {
                if let Some(cc) = c_compiler_for(&tc.cxx) {
                    args.push(OsString::from(format!(
                        "-DCMAKE_C_COMPILER={}",
                        cc.display()
                    )));
                }
            }
        }
    }
    for (key, value) in &spec.options {
        let key = key.strip_prefix("-D").unwrap_or(key);
        args.push(OsString::from(format!("-D{key}={value}")));
    }
    args
}

/// `cmake --build dir --config type [--target t …] [--parallel N]`.
pub(crate) fn build_args(spec: &AdapterSpec, ctx: &AdapterCtx) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--build"),
        ctx.build_dir.as_os_str().to_os_string(),
        OsString::from("--config"),
        OsString::from(&ctx.build_type),
    ];
    for target in &spec.targets {
        args.push(OsString::from("--target"));
        args.push(OsString::from(target));
    }
    if let Some(jobs) = ctx.jobs {
        args.push(OsString::from("--parallel"));
        args.push(OsString::from(jobs.to_string()));
    }
    args
}

/// Best-effort C compiler next to a C++ driver (`clang++` → `clang`,
/// `*g++*` → `*gcc*`, `cl` → itself). Returns `None` when no sibling
/// exists, leaving CMake's own C-compiler discovery in place.
fn c_compiler_for(cxx: &Path) -> Option<PathBuf> {
    let name = cxx.file_name()?.to_string_lossy().into_owned();
    let stem = name
        .strip_suffix(".exe")
        .unwrap_or(&name)
        .to_ascii_lowercase();
    if stem == "cl" {
        return Some(cxx.to_path_buf());
    }
    let sibling = if let Some(rest) = stem.strip_prefix("clang++") {
        format!("clang{rest}")
    } else if stem.contains("g++") {
        stem.replace("g++", "gcc")
    } else {
        return None;
    };
    let mut cand = cxx.to_path_buf();
    cand.set_file_name(format!(
        "{sibling}{}",
        cxx.extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default()
    ));
    cand.is_file().then_some(cand)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::tool_available;
    use rivet_toolchain::{CxxToolchain, ToolchainKind};

    fn spec(targets: &[&str], options: &[(&str, &str)]) -> AdapterSpec {
        AdapterSpec {
            tool: "cmake".to_string(),
            targets: targets.iter().map(|s| s.to_string()).collect(),
            options: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn args_to_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn fake_toolchain() -> CxxToolchain {
        CxxToolchain {
            kind: ToolchainKind::Gcc,
            cxx: PathBuf::from("/usr/bin/g++"),
            version: "test".to_string(),
            msvc_env: None,
            ar: None,
            target: None,
            found_via: "test".to_string(),
        }
    }

    #[test]
    fn configure_args_shape() {
        let ctx = AdapterCtx::new(PathBuf::from("build")).with_build_type("Release");
        let s = spec(&[], &[("MINI_FEATURE", "ON"), ("-DALREADY", "1")]);
        let args = args_to_strings(&configure_args(Path::new("src"), &s, &ctx));
        assert!(args.contains(&"-S".to_string()), "got: {args:?}");
        assert!(args.contains(&"src".to_string()), "got: {args:?}");
        assert!(args.contains(&"-B".to_string()), "got: {args:?}");
        assert!(
            args.iter().any(|a| a == "-DCMAKE_BUILD_TYPE=Release"),
            "got: {args:?}"
        );
        assert!(
            args.iter()
                .any(|a| a.starts_with("-DCMAKE_INSTALL_PREFIX=")),
            "got: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "-DMINI_FEATURE=ON"),
            "got: {args:?}"
        );
        // A user-supplied `-D` prefix is not doubled.
        assert!(args.iter().any(|a| a == "-DALREADY=1"), "got: {args:?}");
        // No toolchain attached: no compiler forwarding.
        assert!(
            !args.iter().any(|a| a.starts_with("-DCMAKE_CXX_COMPILER=")),
            "got: {args:?}"
        );
    }

    #[test]
    fn configure_args_forward_toolchain_compilers() {
        let ctx = AdapterCtx::new(PathBuf::from("build")).with_toolchain(fake_toolchain());
        let args = args_to_strings(&configure_args(Path::new("src"), &spec(&[], &[]), &ctx));
        assert!(
            args.iter()
                .any(|a| a == "-DCMAKE_CXX_COMPILER=/usr/bin/g++"),
            "got: {args:?}"
        );
        // Explicit spec options win over forwarding.
        let s = spec(&[], &[("CMAKE_CXX_COMPILER", "/custom/c++")]);
        let args = args_to_strings(&configure_args(Path::new("src"), &s, &ctx));
        assert!(
            args.iter().any(|a| a == "-DCMAKE_CXX_COMPILER=/custom/c++"),
            "got: {args:?}"
        );
        assert_eq!(
            args.iter()
                .filter(|a| a.starts_with("-DCMAKE_CXX_COMPILER="))
                .count(),
            1,
            "got: {args:?}"
        );
    }

    #[test]
    fn build_args_shape() {
        let ctx = AdapterCtx::new(PathBuf::from("b"))
            .with_build_type("Debug")
            .with_jobs(8);
        let args = args_to_strings(&build_args(&spec(&["ta", "tb"], &[]), &ctx));
        assert_eq!(
            args,
            vec![
                "--build",
                "b",
                "--config",
                "Debug",
                "--target",
                "ta",
                "--target",
                "tb",
                "--parallel",
                "8",
            ],
            "got: {args:?}"
        );
        // No targets, no jobs: plain default-target build.
        let ctx = AdapterCtx::new(PathBuf::from("b"));
        let args = args_to_strings(&build_args(&spec(&[], &[]), &ctx));
        assert_eq!(
            args,
            vec!["--build", "b", "--config", "Debug"],
            "got: {args:?}"
        );
    }

    #[test]
    fn empty_build_type_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = AdapterCtx::new(dir.path().join("build")).with_build_type("  ");
        let err = build(dir.path(), &spec(&[], &[]), &ctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("build type"), "got: {err}");
    }

    /// Two-target fixture: `ma` (type from `BUILD_SHARED_LIBS`) + `mb`
    /// (always static), public headers under `include/`.
    fn write_pair_fixture(src: &Path) {
        std::fs::create_dir_all(src.join("src")).unwrap();
        std::fs::create_dir_all(src.join("include").join("pair")).unwrap();
        std::fs::write(
            src.join("CMakeLists.txt"),
            b"cmake_minimum_required(VERSION 3.16)\n\
              project(pair CXX)\n\
              add_library(ma src/a.cpp)\n\
              add_library(mb STATIC src/b.cpp)\n\
              target_include_directories(ma PUBLIC include)\n\
              target_include_directories(mb PUBLIC include)\n",
        )
        .unwrap();
        std::fs::write(
            src.join("include").join("pair").join("pair.h"),
            b"#pragma once\nint ma_fn();\nint mb_fn();\n",
        )
        .unwrap();
        std::fs::write(
            src.join("src").join("a.cpp"),
            b"#include \"pair/pair.h\"\nint ma_fn() { return 1; }\n",
        )
        .unwrap();
        std::fs::write(
            src.join("src").join("b.cpp"),
            b"#include \"pair/pair.h\"\nint mb_fn() { return 2; }\n",
        )
        .unwrap();
    }

    /// Fixture-aware: `mb` is always static and only `ma` can be shared,
    /// so `mb`'s archive (or any `.a`) counts as static while `ma`'s
    /// import `.lib` does not.
    fn is_static(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        lower.ends_with(".a") || lower.ends_with("mb.lib")
    }

    #[test]
    fn cmake_builds_pair_and_prefers_static() {
        if !tool_available("cmake") {
            eprintln!("SKIP cmake_builds_pair_and_prefers_static: cmake not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        write_pair_fixture(&src);
        // `BUILD_SHARED_LIBS=ON` makes `ma` shared (default would be
        // static); `mb` stays static. Expect static-first link order.
        let ctx = AdapterCtx::new(dir.path().join("build"));
        let s = spec(&[], &[("BUILD_SHARED_LIBS", "ON")]);
        let out = build(&src, &s, &ctx).unwrap();
        assert!(out.libs.len() >= 2, "libs: {:?}", out.libs);
        // Every reported file exists and is absolute.
        for lib in &out.libs {
            match lib {
                crate::LibLink::File(p) => {
                    assert!(p.is_absolute(), "not absolute: {}", p.display());
                    assert!(p.is_file(), "missing: {}", p.display());
                }
                crate::LibLink::Name(n) => panic!("unexpected bare name: {n}"),
            }
        }
        // Static archive links first (shared `ma` sorts after).
        assert!(
            is_static(&out.libs[0].to_string()),
            "first lib should be static, got: {:?}",
            out.libs
        );
        // Source `include/` is exposed; lib dirs exist.
        assert!(
            out.include_dirs.iter().any(|d| d == &src.join("include")),
            "includes: {:?}",
            out.include_dirs
        );
        assert!(
            out.lib_dirs.iter().all(|d| d.is_dir()),
            "lib dirs: {:?}",
            out.lib_dirs
        );
        assert!(out.defines.is_empty());
    }

    #[test]
    fn cmake_options_and_targets_select_one_static_lib() {
        if !tool_available("cmake") {
            eprintln!("SKIP cmake_options_and_targets_select_one_static_lib: cmake not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        write_pair_fixture(&src);
        // `-DBUILD_SHARED_LIBS=OFF` forces `ma` static; `--target ma`
        // skips `mb` entirely.
        let ctx = AdapterCtx::new(dir.path().join("build"));
        let s = spec(&["ma"], &[("BUILD_SHARED_LIBS", "OFF")]);
        let out = build(&src, &s, &ctx).unwrap();
        assert_eq!(out.libs.len(), 1, "libs: {:?}", out.libs);
        let name = out.libs[0].to_string().to_ascii_lowercase();
        assert!(
            name.ends_with("ma.lib") || name.ends_with("libma.a"),
            "expected static ma, got: {name}"
        );
    }
}
