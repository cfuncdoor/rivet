//! Installed-library probes for sourceless (`system: true`) deps.
//!
//! Probing order for tool `system`: `pkg-config --cflags/--libs <name>`
//! when the binary exists, falling back to a well-known include/lib dir
//! search when pkg-config is absent *or* does not know the package. Tool
//! `pkg-config` is the strict first step only (no dir fallback).
//!
//! The only code ever executed here is `pkg-config` itself (a query tool,
//! never a build script). The dir search only stats the filesystem.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::discover::lib_dirs_of;
use super::util::{require_tool, run_capture, tool_available};
use super::{AdapterOutput, Error, LibLink};

/// Strict `pkg-config --cflags/--libs <package>` probe.
pub fn probe_pkg_config(package: &str) -> Result<AdapterOutput, Error> {
    check_package_name(package)?;
    let program = require_tool("pkg-config")?;
    probe_pkg_config_with(&program, package, &[])
}

/// `system`-tool probe: pkg-config first (when present), then the
/// well-known-dir search.
pub fn probe_system(package: &str) -> Result<AdapterOutput, Error> {
    check_package_name(package)?;
    let pkg_config = if tool_available("pkg-config") {
        require_tool("pkg-config").ok()
    } else {
        None
    };
    probe_system_with_roots(
        package,
        &default_include_roots(),
        &default_lib_roots(),
        pkg_config.as_deref(),
        &[],
    )
}

/// Search `include_roots`/`lib_roots` for `package` (no pkg-config).
/// Platform defaults live in [`default_include_roots`] /
/// [`default_lib_roots`]; tests inject tempdir roots.
pub fn search_well_known(package: &str) -> Result<AdapterOutput, Error> {
    check_package_name(package)?;
    search_roots_for(package, &default_include_roots(), &default_lib_roots())
}

fn check_package_name(package: &str) -> Result<(), Error> {
    if package.trim().is_empty() {
        return Err(Error::InvalidSpec {
            message: "system probe needs a non-empty package name. Fix: probe the dependency key (e.g. `zlib`)".to_string(),
        });
    }
    Ok(())
}

/// Core `system` logic with injectable roots, pkg-config program, and
/// child env (the `PKG_CONFIG_PATH` fixture seam for tests).
pub(crate) fn probe_system_with_roots(
    package: &str,
    include_roots: &[PathBuf],
    lib_roots: &[PathBuf],
    pkg_config: Option<&Path>,
    extra_env: &[(&str, &str)],
) -> Result<AdapterOutput, Error> {
    if let Some(program) = pkg_config {
        match probe_pkg_config_with(program, package, extra_env) {
            Ok(out) => return Ok(out),
            // pkg-config ran but does not know the package (or cannot
            // run at all): fall through to the dir search below.
            Err(Error::SystemNotFound { .. }) | Err(Error::CommandFailed { .. }) => {}
            Err(other) => return Err(other),
        }
    }
    search_roots_for(package, include_roots, lib_roots)
}

/// Run `pkg-config --print-errors --cflags|--libs <package>` and parse
/// both outputs. A failing query (unknown package) is
/// [`Error::SystemNotFound`] naming the package.
pub(crate) fn probe_pkg_config_with(
    program: &Path,
    package: &str,
    extra_env: &[(&str, &str)],
) -> Result<AdapterOutput, Error> {
    // A missing binary here means TOCTOU weirdness (presence was checked);
    // surface it as "not found" so `system` probing can still try dirs.
    if program.is_absolute() && !program.is_file() {
        return Err(Error::SystemNotFound {
            package: package.to_string(),
            detail: format!(
                "pkg-config binary `{}` is not usable. {}",
                program.display(),
                search_hint(package)
            ),
        });
    }
    let mut cflags = Command::new(program);
    cflags.args(["--print-errors", "--cflags", package]);
    let cflags_out = run_capture(
        &mut cflags,
        &format!("query pkg-config cflags for `{package}`"),
        "install the library's development package so pkg-config finds its `.pc` file (check `PKG_CONFIG_PATH`)",
        extra_env,
    )
    .map_err(|e| into_not_found(package, e))?;
    let mut libs = Command::new(program);
    libs.args(["--print-errors", "--libs", package]);
    let libs_out = run_capture(
        &mut libs,
        &format!("query pkg-config libs for `{package}`"),
        "install the library's development package so pkg-config finds its `.pc` file (check `PKG_CONFIG_PATH`)",
        extra_env,
    )
    .map_err(|e| into_not_found(package, e))?;
    Ok(parse_pkg_config(&cflags_out, &libs_out))
}

/// Map any pkg-config query failure to [`Error::SystemNotFound`].
/// The detail always names `PKG_CONFIG_PATH` so the fix is actionable even
/// when pkg-config's own stderr wording changes (or the binary failed to
/// spawn at all, e.g. a non-executable `pkg-config` shim on Windows).
fn into_not_found(package: &str, err: Error) -> Error {
    match err {
        Error::CommandFailed { output, .. } => Error::SystemNotFound {
            package: package.to_string(),
            detail: format!(
                "pkg-config does not know `{package}` ({output}). Check `PKG_CONFIG_PATH` for its `.pc` file. {}",
                search_hint(package)
            ),
        },
        other => other,
    }
}

/// Parse `pkg-config --cflags` + `--libs` token streams into the output
/// model. `-I` → include dirs, `-D` → defines, `-L` → lib dirs,
/// `-l` → bare names, absolute library paths → files. Linker-only flags
/// (`-pthread`, `-framework`, `-Wl,…`, `-m…`) carry no rivet meaning and
/// are ignored.
pub fn parse_pkg_config(cflags: &str, libs: &str) -> AdapterOutput {
    let mut out = AdapterOutput::default();
    let mut tokens = cflags.split_whitespace().peekable();
    while let Some(tok) = tokens.next() {
        if let Some(dir) = tok.strip_prefix("-I") {
            let dir = if dir.is_empty() {
                tokens.next().unwrap_or_default().to_string()
            } else {
                dir.to_string()
            };
            if !dir.is_empty() {
                push_unique_path(&mut out.include_dirs, PathBuf::from(dir));
            }
        } else if let Some(def) = tok.strip_prefix("-D") {
            if !def.is_empty() {
                push_unique(&mut out.defines, def.to_string());
            }
        }
    }
    let mut tokens = libs.split_whitespace().peekable();
    while let Some(tok) = tokens.next() {
        if let Some(dir) = tok.strip_prefix("-L") {
            let dir = if dir.is_empty() {
                tokens.next().unwrap_or_default().to_string()
            } else {
                dir.to_string()
            };
            if !dir.is_empty() {
                push_unique_path(&mut out.lib_dirs, PathBuf::from(dir));
            }
        } else if let Some(name) = tok.strip_prefix("-l") {
            if !name.is_empty() {
                push_unique_link(&mut out.libs, LibLink::Name(name.to_string()));
            }
        } else if is_path_token(tok) {
            push_unique_link(&mut out.libs, LibLink::File(PathBuf::from(tok)));
        }
        // Anything else (-pthread, -framework, -Wl,…, -m…) is ignored.
    }
    out
}

/// `true` for tokens that name a library file on disk.
fn is_path_token(tok: &str) -> bool {
    let lower = tok.to_ascii_lowercase();
    (tok.contains('/') || tok.contains('\\'))
        && (lower.ends_with(".a")
            || lower.ends_with(".lib")
            || lower.ends_with(".so")
            || lower.ends_with(".dll")
            || lower.ends_with(".dylib")
            || lower.contains(".so."))
}

fn push_unique(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

fn push_unique_path(list: &mut Vec<PathBuf>, item: PathBuf) {
    if !list.contains(&item) {
        list.push(item);
    }
}

fn push_unique_link(list: &mut Vec<LibLink>, item: LibLink) {
    if !list.contains(&item) {
        list.push(item);
    }
}

// --- well-known-dir search -------------------------------------------------

/// Header name candidates for `package`: `<name>.h` or a `<name>/` dir
/// holding at least one header (e.g. `openssl/ssl.h`).
fn header_hit(root: &Path, package: &str) -> bool {
    if root.join(format!("{package}.h")).is_file() {
        return true;
    }
    let sub = root.join(package);
    if !sub.is_dir() {
        return false;
    }
    std::fs::read_dir(&sub)
        .map(|entries| {
            entries.flatten().any(|e| {
                e.file_type().is_ok_and(|t| t.is_file())
                    && e.path()
                        .extension()
                        .and_then(|x| x.to_str())
                        .is_some_and(|x| x.eq_ignore_ascii_case("h"))
            })
        })
        .unwrap_or(false)
}

/// Library file name candidates for `package` (top level of a lib dir).
fn lib_hit_name(file_name: &str, package: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    let pkg = package.to_ascii_lowercase();
    lower == format!("lib{pkg}.a")
        || lower == format!("lib{pkg}.so")
        || lower == format!("lib{pkg}.dylib")
        || lower == format!("{pkg}.lib")
        || lower == format!("lib{pkg}.lib")
        || lower == format!("{pkg}.dll")
        || lower == format!("lib{pkg}.dll")
        || (lower.starts_with(&format!("lib{pkg}.so.")))
}

/// Search fixed roots for `package`, returning whatever was found.
/// Header-only results (includes, no libs) and link-only results (libs,
/// no includes) are both returned; finding neither is
/// [`Error::SystemNotFound`].
pub(crate) fn search_roots_for(
    package: &str,
    include_roots: &[PathBuf],
    lib_roots: &[PathBuf],
) -> Result<AdapterOutput, Error> {
    let mut out = AdapterOutput::default();
    for root in include_roots {
        if root.is_dir() && header_hit(root, package) {
            push_unique_path(&mut out.include_dirs, root.clone());
        }
    }
    for root in lib_roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut hits: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| lib_hit_name(n, package))
            })
            .collect();
        hits.sort();
        for hit in hits {
            push_unique_link(&mut out.libs, LibLink::File(hit));
        }
    }
    // Static archives first, matching adapter link-order convention.
    // (A dir search cannot tell import `.lib`s from static ones, so every
    // non-`.a` archive ranks equally.)
    out.libs.sort_by_key(|l| match l {
        LibLink::File(p) => {
            let n = p
                .file_name()
                .and_then(|x| x.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if n.ends_with(".a") {
                0u8
            } else {
                1u8
            }
        }
        LibLink::Name(_) => 2u8,
    });
    out.lib_dirs = lib_dirs_of(&out.libs);
    if out.include_dirs.is_empty() && out.libs.is_empty() {
        return Err(Error::SystemNotFound {
            package: package.to_string(),
            detail: format!(
                "searched include dirs [{}] and lib dirs [{}]. {}",
                display_roots(include_roots),
                display_roots(lib_roots),
                search_hint(package)
            ),
        });
    }
    Ok(out)
}

fn display_roots(roots: &[PathBuf]) -> String {
    if roots.is_empty() {
        return "<none>".to_string();
    }
    roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn search_hint(package: &str) -> String {
    format!(
        "Fix: install the `{package}` development package (Linux: `sudo apt install lib{package}-dev`; macOS: `brew install {package}`; Windows: use vcpkg or MSYS2) so it is visible to pkg-config or lives in a standard include/lib dir"
    )
}

/// Standard include roots, plus `INCLUDE` env entries (MSVC Developer
/// Prompt / vcvars shells) first so the active toolchain wins.
pub fn default_include_roots() -> Vec<PathBuf> {
    let mut roots = env_paths("INCLUDE");
    #[cfg(unix)]
    roots.extend(
        [
            "/usr/include",
            "/usr/local/include",
            "/opt/local/include",
            "/opt/homebrew/include",
            "/usr/local/opt",
        ]
        .iter()
        .map(PathBuf::from),
    );
    #[cfg(windows)]
    roots.extend(
        [
            "C:\\Strawberry\\c\\include",
            "C:\\msys64\\usr\\include",
            "C:\\msys64\\mingw64\\include",
        ]
        .iter()
        .map(PathBuf::from),
    );
    dedup_existing_or_all(roots)
}

/// Standard lib roots, plus `LIB` env entries first.
pub fn default_lib_roots() -> Vec<PathBuf> {
    let mut roots = env_paths("LIB");
    #[cfg(unix)]
    roots.extend(
        [
            "/usr/lib",
            "/usr/lib64",
            "/usr/lib/x86_64-linux-gnu",
            "/usr/lib/aarch64-linux-gnu",
            "/usr/local/lib",
            "/opt/local/lib",
            "/opt/homebrew/lib",
        ]
        .iter()
        .map(PathBuf::from),
    );
    #[cfg(windows)]
    roots.extend(
        [
            "C:\\Strawberry\\c\\lib",
            "C:\\msys64\\usr\\lib",
            "C:\\msys64\\mingw64\\lib",
        ]
        .iter()
        .map(PathBuf::from),
    );
    dedup_existing_or_all(roots)
}

/// Split a path-list env var (`;` on Windows, `:` elsewhere).
fn env_paths(var: &str) -> Vec<PathBuf> {
    std::env::var_os(var)
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default()
}

/// Dedupe roots, keeping entries whether or not they exist (missing dirs
/// are skipped at search time; keeping them makes "not found" errors show
/// what was searched).
fn dedup_existing_or_all(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots.sort();
    roots.dedup();
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pkg_config_tokens() {
        let out = parse_pkg_config(
            "-I/tmp/inc -DMINI_STATIC -DFOO=1 -pthread",
            "-L/tmp/lib -lmini /tmp/lib/libextra.a -pthread -Wl,-rpath,/x -framework CoreFoundation",
        );
        assert_eq!(out.include_dirs, vec![PathBuf::from("/tmp/inc")]);
        assert_eq!(
            out.defines,
            vec!["MINI_STATIC".to_string(), "FOO=1".to_string()]
        );
        assert_eq!(out.lib_dirs, vec![PathBuf::from("/tmp/lib")]);
        assert_eq!(
            out.libs,
            vec![
                LibLink::Name("mini".to_string()),
                LibLink::File(PathBuf::from("/tmp/lib/libextra.a")),
            ]
        );
        // Split `-I /dir` form.
        let out = parse_pkg_config("-I /tmp/a -D", "-L /tmp/b");
        assert_eq!(out.include_dirs, vec![PathBuf::from("/tmp/a")]);
        assert!(out.defines.is_empty());
        assert_eq!(out.lib_dirs, vec![PathBuf::from("/tmp/b")]);
    }

    /// Write a minimal `.pc` fixture resolveable via `PKG_CONFIG_PATH`.
    fn write_pc(dir: &Path, name: &str) {
        let prefix = dir.join("prefix");
        std::fs::create_dir_all(prefix.join("include")).unwrap();
        std::fs::create_dir_all(prefix.join("lib")).unwrap();
        std::fs::write(prefix.join("include").join(format!("{name}.h")), b"x").unwrap();
        std::fs::write(prefix.join("lib").join(format!("lib{name}.a")), b"x").unwrap();
        std::fs::write(
            dir.join(format!("{name}.pc")),
            format!(
                "prefix={}\n\
                 includedir=${{prefix}}/include\n\
                 libdir=${{prefix}}/lib\n\
                 \n\
                 Name: {name}\n\
                 Description: fixture\n\
                 Version: 1.0\n\
                 Cflags: -I${{includedir}} -D{name}_STATIC\n\
                 Libs: -L${{libdir}} -l{name}\n",
                prefix.display()
            ),
        )
        .unwrap();
    }

    /// `true` when `program` actually runs (`--version` succeeds).
    /// `require_tool` only proves the file exists on `PATH`; on some
    /// Windows runners that file is a non-executable shim (spawn fails
    /// with OS error 193), so fixture tests must skip instead of failing.
    fn pkg_config_runnable(program: &Path) -> bool {
        std::process::Command::new(program)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn pkg_config_fixture_probed_with_path_override() {
        let Some(pc) = require_tool("pkg-config").ok() else {
            eprintln!("SKIP pkg_config_fixture_probed_with_path_override: pkg-config not on PATH");
            return;
        };
        if !pkg_config_runnable(&pc) {
            eprintln!("SKIP pkg_config_fixture_probed_with_path_override: pkg-config not runnable");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        write_pc(dir.path(), "minifig");
        let pcp = dir.path().to_string_lossy().into_owned();
        let out =
            probe_pkg_config_with(&pc, "minifig", &[("PKG_CONFIG_PATH", pcp.as_str())]).unwrap();
        assert!(
            out.include_dirs.iter().any(|d| d.ends_with("include")),
            "includes: {:?}",
            out.include_dirs
        );
        assert_eq!(out.defines, vec!["minifig_STATIC".to_string()]);
        assert!(
            out.lib_dirs.iter().any(|d| d.ends_with("lib")),
            "lib dirs: {:?}",
            out.lib_dirs
        );
        assert_eq!(out.libs, vec![LibLink::Name("minifig".to_string())]);
    }

    #[test]
    fn pkg_config_unknown_package_names_it() {
        let Ok(pc) = require_tool("pkg-config") else {
            eprintln!("SKIP pkg_config_unknown_package_names_it: pkg-config not on PATH");
            return;
        };
        let err = probe_pkg_config_with(&pc, "rivet-no-such-pkg-xyz", &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("rivet-no-such-pkg-xyz"), "got: {err}");
        assert!(err.contains("PKG_CONFIG_PATH"), "got: {err}");
    }

    #[test]
    fn dir_search_finds_fixture_and_names_misses() {
        let dir = tempfile::tempdir().unwrap();
        let inc = dir.path().join("include");
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(&inc).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(inc.join("vendored.h"), b"x").unwrap();
        std::fs::write(lib.join("libvendored.a"), b"x").unwrap();
        // A `<name>/` subdir with headers also hits.
        std::fs::create_dir_all(inc.join("ssl-ish")).unwrap();
        std::fs::write(inc.join("ssl-ish").join("ssl.h"), b"x").unwrap();

        let out = search_roots_for(
            "vendored",
            std::slice::from_ref(&inc),
            std::slice::from_ref(&lib),
        )
        .unwrap();
        assert_eq!(out.include_dirs, vec![inc.clone()]);
        assert_eq!(out.lib_dirs, vec![lib.clone()]);
        assert_eq!(out.libs, vec![LibLink::File(lib.join("libvendored.a"))]);

        let out = search_roots_for("ssl-ish", std::slice::from_ref(&inc), &[]).unwrap();
        assert_eq!(out.include_dirs, vec![inc.clone()]);
        assert!(out.libs.is_empty(), "header-only is fine: {:?}", out.libs);

        let err = search_roots_for("ghost", &[inc], &[lib])
            .unwrap_err()
            .to_string();
        assert!(err.contains("ghost"), "got: {err}");
        assert!(err.contains("Fix:"), "got: {err}");
    }

    #[test]
    fn system_falls_back_to_dir_search() {
        let dir = tempfile::tempdir().unwrap();
        let inc = dir.path().join("include");
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(&inc).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(inc.join("fallback.h"), b"x").unwrap();
        std::fs::write(lib.join("libfallback.a"), b"x").unwrap();
        // A pkg-config program that cannot run is treated as "no
        // pkg-config" and the dir search still delivers.
        let out = probe_system_with_roots(
            "fallback",
            &[inc],
            &[lib],
            Some(Path::new("rivet-no-such-pkg-config-xyz")),
            &[],
        )
        .unwrap();
        assert!(!out.libs.is_empty());
    }

    #[test]
    fn system_with_real_pkg_config_prefers_it() {
        let Ok(pc) = require_tool("pkg-config") else {
            eprintln!("SKIP system_with_real_pkg_config_prefers_it: pkg-config not on PATH");
            return;
        };
        if !pkg_config_runnable(&pc) {
            eprintln!("SKIP system_with_real_pkg_config_prefers_it: pkg-config not runnable");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        write_pc(dir.path(), "sysfig");
        let pcp = dir.path().to_string_lossy().into_owned();
        // Empty dir roots: only pkg-config can answer.
        let out = probe_system_with_roots(
            "sysfig",
            &[],
            &[],
            Some(&pc),
            &[("PKG_CONFIG_PATH", pcp.as_str())],
        )
        .unwrap();
        assert_eq!(out.libs, vec![LibLink::Name("sysfig".to_string())]);
    }
}
