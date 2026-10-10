//! Meson adapter (best-effort): `meson setup` + `meson compile`.
//!
//! ```text
//! meson setup <build> <src> --buildtype=<debug|release> -Db_vscrt=mt
//!             [-Dkey=value ...]
//! meson setup --reconfigure <build> [-Dkey=value ...]   # when configured
//! meson compile -C <build> [targets ...]
//! ```
//! Only the `meson` binary is ever executed. A missing `meson` is
//! [`Error::ToolMissing`] — never a silent fallback to another tool.
//!
//! The `b_vscrt=mt` default pins the static CRT to match rivet's own
//! objects (driver default `/MT`); it is inert off-MSVC and removable
//! by overriding `b_vscrt` in `options`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use rivet_manifest::AdapterSpec;

use super::discover::{discover_include_dirs, discover_libs, lib_dirs_of};
use super::util::{require_tool, run_capture};
use super::{AdapterCtx, AdapterOutput, Error};

/// Marker proving `build_dir` was already set up (present for every
/// backend after `meson setup` succeeds).
fn configured_marker(build_dir: &Path) -> PathBuf {
    build_dir.join("meson-info").join("meson-info.json")
}

/// Set up + compile `source_dir` per `spec`, discovering outputs under
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
    let meson = require_tool("meson")?;
    std::fs::create_dir_all(&ctx.build_dir)?;

    let mut setup = Command::new(&meson);
    setup.args(setup_args(source_dir, spec, ctx));
    if let Some(tc) = &ctx.toolchain {
        tc.apply_env(&mut setup);
    }
    run_capture(
        &mut setup,
        &format!("set up {} with meson", source_dir.display()),
        "inspect the foreign meson.build (missing `project()`, unknown `-D` option, or no compiler/backend found) and retry",
        &[],
    )?;

    let mut compile = Command::new(&meson);
    compile.args(compile_args(spec, ctx));
    if let Some(tc) = &ctx.toolchain {
        tc.apply_env(&mut compile);
    }
    run_capture(
        &mut compile,
        &format!("compile {} with meson", ctx.build_dir.display()),
        "read the compiler errors above — they come from the foreign project's own sources",
        &[],
    )?;

    let libs = discover_libs(&ctx.build_dir, &spec.targets)?;
    let mut lib_dirs = lib_dirs_of(&libs);
    lib_dirs.extend(super::discover::dll_dirs(&ctx.build_dir));
    lib_dirs.sort();
    lib_dirs.dedup();
    let include_dirs = discover_include_dirs(source_dir, &ctx.build_dir);
    Ok(AdapterOutput {
        include_dirs,
        lib_dirs,
        libs,
        defines: Vec::new(),
    })
}

/// `meson setup` args: fresh setup takes `<build> <src>`, an already
/// configured dir takes `--reconfigure <build>` (srcdir would error).
pub(crate) fn setup_args(source_dir: &Path, spec: &AdapterSpec, ctx: &AdapterCtx) -> Vec<OsString> {
    let mut args = vec![OsString::from("setup")];
    if configured_marker(&ctx.build_dir).is_file() {
        args.push(OsString::from("--reconfigure"));
        args.push(ctx.build_dir.as_os_str().to_os_string());
    } else {
        args.push(ctx.build_dir.as_os_str().to_os_string());
        args.push(source_dir.as_os_str().to_os_string());
    }
    args.push(OsString::from(format!(
        "--buildtype={}",
        meson_buildtype(&ctx.build_type)
    )));
    // Static CRT to match rivet's objects (see module docs); a spec
    // `b_vscrt` wins.
    if !spec.options.contains_key("b_vscrt") {
        args.push(OsString::from("-Db_vscrt=mt"));
    }
    for (key, value) in &spec.options {
        args.push(OsString::from(format!("-D{key}={value}")));
    }
    args
}

/// `meson compile -C <build> [targets …]`. Jobs are left to the backend
/// default (ninja parallelizes on its own).
pub(crate) fn compile_args(spec: &AdapterSpec, ctx: &AdapterCtx) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("compile"),
        OsString::from("-C"),
        ctx.build_dir.as_os_str().to_os_string(),
    ];
    for target in &spec.targets {
        args.push(OsString::from(target));
    }
    args
}

/// Map rivet `Debug`/`Release` to meson buildtypes; anything else passes
/// through lowercased and meson validates it.
fn meson_buildtype(build_type: &str) -> String {
    match build_type.to_ascii_lowercase().as_str() {
        "debug" => "debug".to_string(),
        "release" => "release".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::tool_available;

    fn spec(targets: &[&str], options: &[(&str, &str)]) -> AdapterSpec {
        AdapterSpec {
            tool: "meson".to_string(),
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

    #[test]
    fn setup_args_fresh_vs_reconfigure() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = AdapterCtx::new(dir.path().join("build"));
        let args = args_to_strings(&setup_args(
            Path::new("src"),
            &spec(&[], &[("werror", "false")]),
            &ctx,
        ));
        assert_eq!(
            args[..4],
            vec![
                "setup",
                &dir.path().join("build").to_string_lossy().as_ref(),
                "src",
                "--buildtype=debug",
            ],
            "got: {args:?}"
        );
        assert!(
            args.contains(&"-Dwerror=false".to_string()),
            "got: {args:?}"
        );
        // Static CRT default (matches rivet's own objects).
        assert!(args.contains(&"-Db_vscrt=mt".to_string()), "got: {args:?}");

        // With the configured marker present, srcdir is dropped in favor
        // of --reconfigure.
        let build = dir.path().join("build2");
        std::fs::create_dir_all(build.join("meson-info")).unwrap();
        std::fs::write(build.join("meson-info").join("meson-info.json"), b"{}").unwrap();
        let ctx = AdapterCtx::new(build.clone()).with_build_type("Release");
        let args = args_to_strings(&setup_args(Path::new("src"), &spec(&[], &[]), &ctx));
        assert!(!args.contains(&"src".to_string()), "got: {args:?}");
        assert!(args.contains(&"--reconfigure".to_string()), "got: {args:?}");
        assert!(
            args.contains(&"--buildtype=release".to_string()),
            "got: {args:?}"
        );
    }

    #[test]
    fn compile_args_shape() {
        let ctx = AdapterCtx::new(PathBuf::from("b"));
        let args = args_to_strings(&compile_args(&spec(&["ma", "mb"], &[]), &ctx));
        assert_eq!(
            args,
            vec!["compile", "-C", "b", "ma", "mb"],
            "got: {args:?}"
        );
    }

    #[test]
    fn meson_buildtype_mapping() {
        assert_eq!(meson_buildtype("Debug"), "debug");
        assert_eq!(meson_buildtype("Release"), "release");
        assert_eq!(meson_buildtype("plain"), "plain");
    }

    #[test]
    fn vscrt_override_wins() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = AdapterCtx::new(dir.path().join("build"));
        let args = args_to_strings(&setup_args(
            Path::new("src"),
            &spec(&[], &[("b_vscrt", "mtd")]),
            &ctx,
        ));
        assert_eq!(
            args.iter().filter(|a| a.starts_with("-Db_vscrt=")).count(),
            1,
            "got: {args:?}"
        );
        assert!(args.contains(&"-Db_vscrt=mtd".to_string()), "got: {args:?}");
    }

    #[test]
    fn meson_missing_or_misconfigured_is_actionable() {
        if !tool_available("meson") {
            let dir = tempfile::tempdir().unwrap();
            let ctx = AdapterCtx::new(dir.path().join("build"));
            let err = build(dir.path(), &spec(&[], &[]), &ctx)
                .unwrap_err()
                .to_string();
            assert!(err.contains("meson"), "got: {err}");
            assert!(err.contains("Fix:"), "got: {err}");
        } else {
            // Meson present: a srcdir without meson.build fails at setup
            // with the tool's own actionable output attached.
            let dir = tempfile::tempdir().unwrap();
            let ctx = AdapterCtx::new(dir.path().join("build"));
            let err = build(dir.path(), &spec(&[], &[]), &ctx)
                .unwrap_err()
                .to_string();
            assert!(err.contains("meson"), "got: {err}");
        }
    }
}
