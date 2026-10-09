//! Build scripts: compile and run `build.rivet.cpp` before the main build
//! and apply the `rivet:` directives it prints (include dirs, link libs,
//! defines, rerun-if-changed / rerun-if-env).
//!
//! Convention: a file named [`SCRIPT_FILE_NAME`] (`build.rivet.cpp`) at the
//! package root is auto-detected ([`has_script`]). The coordinator compiles it
//! with [`compile_script`], runs the result with [`run_script`] (or both at
//! once with [`run_package_script`]), feeds the returned [`ScriptOutput`]
//! into the graph's include/define/lib inputs, and gates rebuilds with
//! [`needs_rerun`].
//!
//! Sandbox limits (recorded honestly, see the M7 decision entry): the script
//! runs as the current user with its working directory set to the package
//! dir and a scrubbed environment (secret-looking vars stripped — see
//! [`run_script`]). There is **no** network or filesystem isolation on stock
//! OSes; a malicious build script can read the user's files. Only run build
//! scripts from sources you trust, same as Cargo's `build.rs` model.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rivet_toolchain::{CompileOptions, CxxToolchain};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Build-script file-name convention: `<package-dir>/build.rivet.cpp`.
pub const SCRIPT_FILE_NAME: &str = "build.rivet.cpp";

/// Stdout line prefix introducing a directive (`rivet:<key> <value>`).
pub const DIRECTIVE_PREFIX: &str = "rivet:";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Build script errors. Always actionable: what failed, where, how to fix.
#[derive(Debug, Error)]
pub enum Error {
    /// No `build.rivet.cpp` where one was expected.
    #[error("no build script at {path}. Fix: create `{name}` at the package root (or skip build-script handling when none exists)")]
    MissingScript {
        /// Path that was probed.
        path: String,
        /// [`SCRIPT_FILE_NAME`].
        name: &'static str,
    },
    /// A child process (compiler, linker, or the script itself) could not be
    /// started at all.
    #[error("could not start {what}: {message}. Fix: check `rivet toolchains` output and that the file exists")]
    StartFailed {
        /// What was being started.
        what: String,
        /// OS error text.
        message: String,
    },
    /// Compiling or linking `build.rivet.cpp` failed (diagnostics captured).
    #[error("build script {script}: {stage} failed with captured output below.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\nFix: fix the errors above in {script} (it must compile as plain C++17-or-newer with the package `include/` on the path)")]
    CompileFailed {
        /// Script path as passed to [`compile_script`].
        script: String,
        /// `"compile"` or `"link"`.
        stage: &'static str,
        /// Captured stdout (may be empty).
        stdout: String,
        /// Captured stderr (may be empty).
        stderr: String,
    },
    /// The script executable exited non-zero (output captured).
    #[error("build script {exe} exited with {status}.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}\nFix: run it by hand in the package dir to reproduce, then fix the script")]
    RunFailed {
        /// Script executable path.
        exe: String,
        /// Exit status text.
        status: String,
        /// Captured stdout.
        stdout: String,
        /// Captured stderr.
        stderr: String,
    },
    /// A `rivet:` line names an unknown directive key.
    #[error("build script output line {line_no}: unknown directive `{key}` in `{line}`. Fix: use one of `include`, `lib`, `libdir`, `define`, `rerun-if-changed`, `rerun-if-env` (directives are lowercase and space-separated, e.g. `rivet:include generated`)")]
    UnknownDirective {
        /// 1-based stdout line number.
        line_no: usize,
        /// Key as written after `rivet:`.
        key: String,
        /// Full line as written.
        line: String,
    },
    /// A `rivet:` line is malformed (no key/value split or an empty/invalid
    /// value).
    #[error("build script output line {line_no}: `{line}`. Fix: {hint}")]
    MalformedDirective {
        /// 1-based stdout line number.
        line_no: usize,
        /// Full line as written.
        line: String,
        /// What was expected.
        hint: String,
    },
    /// IO failure.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Toolchain failure (e.g. options the toolchain cannot honor).
    #[error("{0}")]
    Toolchain(#[from] rivet_toolchain::Error),
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// Typed output of a build script run.
///
/// Relative directive paths are resolved against the package dir at parse
/// time, so every `PathBuf` here is absolute. Freshness state
/// (`script_hash`, `rerun_file_hashes`, `env_values`) is filled by
/// [`run_script`]; outputs from [`parse_directives`] alone carry empty
/// freshness state and always compare "changed" against a snapshotted one.
#[derive(Debug, Clone, Default)]
pub struct ScriptOutput {
    /// Extra include dirs for the main build (`rivet:include <dir>`).
    pub includes: Vec<PathBuf>,
    /// Extra link inputs (`rivet:lib <file|-lname>`, verbatim, in order).
    pub libs: Vec<String>,
    /// Extra library search dirs (`rivet:libdir <dir>`).
    pub lib_dirs: Vec<PathBuf>,
    /// Extra preprocessor defines (`rivet:define <K[=V]>`, verbatim).
    pub defines: Vec<String>,
    /// Files gating a rerun (`rivet:rerun-if-changed <path>`).
    pub rerun_if_changed: Vec<PathBuf>,
    /// Env vars gating a rerun (`rivet:rerun-if-env <VAR>`).
    pub rerun_if_env: Vec<String>,
    /// Non-directive stdout lines plus stderr lines, in order — the
    /// coordinator passes these through to the build log.
    pub log: Vec<String>,
    /// SHA-256 hex of the `build.rivet.cpp` bytes at snapshot time (`""`
    /// when parsed from text only).
    pub script_hash: String,
    /// Absolute rerun-file path → SHA-256 hex at snapshot time (`None` =
    /// the file was missing).
    pub rerun_file_hashes: BTreeMap<PathBuf, Option<String>>,
    /// Watched var → value at snapshot time (`None` = unset). `OsString`
    /// because Windows env values are not necessarily UTF-8.
    pub env_values: BTreeMap<String, Option<std::ffi::OsString>>,
}

// ---------------------------------------------------------------------------
// Discovery + hashing helpers
// ---------------------------------------------------------------------------

/// Path to the build script for a package dir (`<dir>/build.rivet.cpp`).
pub fn script_path(package_dir: &Path) -> PathBuf {
    package_dir.join(SCRIPT_FILE_NAME)
}

/// `true` when the package carries a `build.rivet.cpp`.
pub fn has_script(package_dir: &Path) -> bool {
    script_path(package_dir).is_file()
}

/// SHA-256 hex of bytes.
pub fn hash_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// SHA-256 hex of a file, or `Ok(None)` when it does not exist. Other IO
/// errors are reported (a vanishing rerun file mid-snapshot is a real
/// problem, not a silent miss).
pub fn hash_file_opt(path: &Path) -> Result<Option<String>, Error> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(hash_bytes(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Effective `-std` for compiling a build script: the requested standard
/// when it parses as C++17 or newer, `c++17` when it parses as older, and
/// the request verbatim when it does not parse (e.g. `c++latest` — assume
/// the caller knows better than us).
pub fn script_std(std: &str) -> String {
    let lower = std.to_ascii_lowercase();
    match lower.find("c++") {
        None => "c++17".to_string(),
        Some(i) => {
            let rest = &lower[i + 3..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                return std.to_string();
            }
            // `c++2a`-style: a lone leading `2` means the C++20 era.
            if digits.len() == 1 && digits.starts_with('2') {
                return std.to_string();
            }
            match digits.parse::<u32>() {
                Ok(n) if n >= 17 => std.to_string(),
                Ok(_) => "c++17".to_string(),
                Err(_) => std.to_string(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Directive parsing
// ---------------------------------------------------------------------------

/// Parse script stdout into a [`ScriptOutput`].
///
/// Lines starting with `rivet:` are directives (`<key>` then whitespace or
/// `=` then the value); everything else lands in [`ScriptOutput::log`].
/// Unknown keys are an error naming the line number and content. Relative
/// paths resolve against `package_dir`. Repeats are deduplicated (first
/// occurrence wins — `-I` order is semantics). Pure: performs no IO, fills
/// no freshness state (see [`run_script`]).
pub fn parse_directives(stdout: &str, package_dir: &Path) -> Result<ScriptOutput, Error> {
    let mut out = ScriptOutput::default();
    for (idx, line) in stdout.lines().enumerate() {
        let line_no = idx + 1;
        let Some(rest) = line.strip_prefix(DIRECTIVE_PREFIX) else {
            out.log.push(line.to_string());
            continue;
        };
        let (key, value) = split_key_value(rest).ok_or_else(|| Error::MalformedDirective {
            line_no,
            line: line.to_string(),
            hint: "expected `rivet:<key> <value>` (e.g. `rivet:include generated`)".to_string(),
        })?;
        if value.is_empty() {
            return Err(Error::MalformedDirective {
                line_no,
                line: line.to_string(),
                hint: format!("`{key}` needs a value (e.g. `rivet:{key} <value>`)"),
            });
        }
        match key {
            "include" | "libdir" => {
                let dir = resolve_against(package_dir, value);
                if key == "include" {
                    out.includes.push(dir);
                } else {
                    out.lib_dirs.push(dir);
                }
            }
            "lib" => out.libs.push(value.to_string()),
            "define" => {
                let name = value.split('=').next().unwrap_or("");
                if name.is_empty() || name.chars().any(|c| c.is_whitespace()) {
                    return Err(Error::MalformedDirective {
                        line_no,
                        line: line.to_string(),
                        hint: "expected `rivet:define <NAME[=value]>`".to_string(),
                    });
                }
                out.defines.push(value.to_string());
            }
            "rerun-if-changed" => out
                .rerun_if_changed
                .push(resolve_against(package_dir, value)),
            "rerun-if-env" => {
                if value.chars().any(|c| c == '=' || c.is_whitespace()) {
                    return Err(Error::MalformedDirective {
                        line_no,
                        line: line.to_string(),
                        hint: "expected `rivet:rerun-if-env <VAR>` (a bare variable name)"
                            .to_string(),
                    });
                }
                out.rerun_if_env.push(value.to_string());
            }
            _ => {
                return Err(Error::UnknownDirective {
                    line_no,
                    key: key.to_string(),
                    line: line.to_string(),
                });
            }
        }
    }
    dedupe_vec(&mut out.includes);
    dedupe_vec(&mut out.libs);
    dedupe_vec(&mut out.lib_dirs);
    dedupe_vec(&mut out.defines);
    dedupe_vec(&mut out.rerun_if_changed);
    dedupe_vec(&mut out.rerun_if_env);
    Ok(out)
}

/// Split `"<key><sep><value>"` where `<sep>` is the first whitespace or `=`
/// run. Returns `None` when there is no separator at all.
fn split_key_value(rest: &str) -> Option<(&str, &str)> {
    let body = rest.trim_start();
    let sep = body.find([' ', '\t', '='])?;
    let key = &body[..sep];
    if key.is_empty() {
        return None;
    }
    let value = body[sep..].trim_start_matches([' ', '\t', '=']).trim();
    Some((key, value))
}

/// Resolve a directive path against the package dir (absolute stays as-is).
fn resolve_against(package_dir: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        package_dir.join(path)
    }
}

/// Remove exact duplicates, keeping first occurrences (order is semantics).
fn dedupe_vec<T>(items: &mut Vec<T>)
where
    T: Eq + std::hash::Hash + Clone,
{
    let mut seen: HashSet<T> = HashSet::new();
    items.retain(|item| seen.insert(item.clone()));
}

// ---------------------------------------------------------------------------
// Compile + run
// ---------------------------------------------------------------------------

/// Compile `build.rivet.cpp` with the detected toolchain and return the
/// script executable path.
///
/// The script builds as C++17 minimum (see [`script_std`]) with the
/// package's `include/` dir appended to the include path when it exists.
/// Output lands in `out_dir` as `build-script[.exe]` (object next to it).
/// Compiler diagnostics are captured and reported verbatim on failure.
pub fn compile_script(
    toolchain: &CxxToolchain,
    script_path: &Path,
    out_dir: &Path,
    opts: &CompileOptions,
) -> Result<PathBuf, Error> {
    if !script_path.is_file() {
        return Err(Error::MissingScript {
            path: script_path.display().to_string(),
            name: SCRIPT_FILE_NAME,
        });
    }
    let package_dir = script_path.parent().unwrap_or_else(|| Path::new("."));
    let mut script_opts = opts.clone();
    script_opts.std = script_std(&opts.std);
    let include_dir = package_dir.join("include");
    if include_dir.is_dir() && !script_opts.include_dirs.contains(&include_dir) {
        script_opts.include_dirs.push(include_dir);
    }
    toolchain.validate_options(&script_opts)?;
    std::fs::create_dir_all(out_dir)?;

    let obj = out_dir.join(format!("build-script.{}", toolchain.obj_extension()));
    let exe = out_dir.join(format!("build-script{}", toolchain.exe_suffix()));

    let mut compile = toolchain.compile_command(script_path, &obj, None, &script_opts);
    let cout = compile.output().map_err(|e| Error::StartFailed {
        what: format!("build-script compile ({})", toolchain.cxx.display()),
        message: e.to_string(),
    })?;
    if !cout.status.success() {
        return Err(Error::CompileFailed {
            script: script_path.display().to_string(),
            stage: "compile",
            stdout: String::from_utf8_lossy(&cout.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&cout.stderr).into_owned(),
        });
    }

    let mut link = toolchain.link_exe_command(
        std::slice::from_ref(&obj),
        &[],
        &[],
        &[],
        &exe,
        &script_opts,
    );
    let lout = link.output().map_err(|e| Error::StartFailed {
        what: format!("build-script link ({})", toolchain.cxx.display()),
        message: e.to_string(),
    })?;
    if !lout.status.success() {
        return Err(Error::CompileFailed {
            script: script_path.display().to_string(),
            stage: "link",
            stdout: String::from_utf8_lossy(&lout.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&lout.stderr).into_owned(),
        });
    }
    Ok(exe)
}

/// `true` for secret-looking env var names: anything ending in `_TOKEN` or
/// `_KEY` (case-insensitive), e.g. `GITHUB_TOKEN`, `CARGO_REGISTRY_TOKEN`,
/// `AWS_SECRET_ACCESS_KEY`. These are stripped before the script runs so a
/// chatty script cannot exfiltrate them through its logged output.
fn is_secret_var(name: &OsStr) -> bool {
    let upper = name.to_string_lossy().to_ascii_uppercase();
    upper.ends_with("_TOKEN") || upper.ends_with("_KEY")
}

/// Fill freshness state: script hash + rerun-file hashes + env snapshot.
fn snapshot_freshness(package_dir: &Path, out: &mut ScriptOutput) -> Result<(), Error> {
    out.script_hash = hash_file_opt(&script_path(package_dir))?.unwrap_or_default();
    out.rerun_file_hashes.clear();
    for path in &out.rerun_if_changed {
        let abs = resolve_against(package_dir, &path.display().to_string());
        out.rerun_file_hashes
            .insert(abs.clone(), hash_file_opt(&abs)?);
    }
    out.env_values.clear();
    for var in &out.rerun_if_env {
        out.env_values.insert(var.clone(), std::env::var_os(var));
    }
    Ok(())
}

/// Run a compiled build script and parse its directives.
///
/// - Working directory is `package_dir`.
/// - The environment is scrubbed of `*_TOKEN` / `*_KEY` vars (see
///   [`is_secret_var`]); everything else passes through unchanged.
/// - stdin is null (a script waiting on stdin would hang the build).
/// - Non-directive stdout lines pass into [`ScriptOutput::log`] (stderr
///   lines too, appended after stdout); with `verbose`, log lines are also
///   echoed to this process's stdout for the build log.
/// - Non-zero exit is an actionable error with captured output.
/// - No network or filesystem isolation is applied (stock-OS limitation —
///   only run scripts from sources you trust).
pub fn run_script(exe: &Path, package_dir: &Path, verbose: bool) -> Result<ScriptOutput, Error> {
    let mut cmd = Command::new(exe);
    cmd.current_dir(package_dir);
    cmd.stdin(Stdio::null());
    for (key, _) in std::env::vars_os() {
        if is_secret_var(&key) {
            cmd.env_remove(&key);
        }
    }
    let out = cmd.output().map_err(|e| Error::StartFailed {
        what: format!("build script {}", exe.display()),
        message: e.to_string(),
    })?;
    if !out.status.success() {
        return Err(Error::RunFailed {
            exe: exe.display().to_string(),
            status: out.status.to_string(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut parsed = parse_directives(&stdout, package_dir)?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stderr.lines() {
        parsed.log.push(line.to_string());
    }
    snapshot_freshness(package_dir, &mut parsed)?;
    if verbose {
        for line in &parsed.log {
            println!("{line}");
        }
    }
    Ok(parsed)
}

/// Compile and run the package's `build.rivet.cpp` in one step.
///
/// Returns `Ok(None)` when the package has no script (auto-detect
/// convention); otherwise compiles into `out_dir` (see [`compile_script`])
/// and runs with [`run_script`].
pub fn run_package_script(
    toolchain: &CxxToolchain,
    package_dir: &Path,
    out_dir: &Path,
    opts: &CompileOptions,
    verbose: bool,
) -> Result<Option<ScriptOutput>, Error> {
    let script = script_path(package_dir);
    if !script.is_file() {
        return Ok(None);
    }
    let exe = compile_script(toolchain, &script, out_dir, opts)?;
    run_script(&exe, package_dir, verbose).map(Some)
}

/// `true` when the script must run again: the `build.rivet.cpp` bytes
/// changed (`script_hash` vs `previous.script_hash`), any watched file's
/// content hash changed (missing ↔ present counts), or any watched env
/// var's value changed. Infallible by design: unreadable state compares
/// "changed" rather than erroring (a rebuild is always safe).
pub fn needs_rerun(package_dir: &Path, previous: &ScriptOutput, script_hash: &str) -> bool {
    if previous.script_hash != script_hash {
        return true;
    }
    for (path, old) in &previous.rerun_file_hashes {
        let abs = if path.is_absolute() {
            path.clone()
        } else {
            package_dir.join(path)
        };
        let current = hash_file_opt(&abs).unwrap_or(None);
        if current != *old {
            return true;
        }
    }
    for var in &previous.rerun_if_env {
        let old: Option<std::ffi::OsString> = previous.env_values.get(var).cloned().flatten();
        if old != std::env::var_os(var) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivet_toolchain::CompileOptions;

    fn pkg_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn parse(text: &str, dir: &Path) -> ScriptOutput {
        parse_directives(text, dir).unwrap()
    }

    #[test]
    fn parse_all_directive_kinds() {
        let dir = pkg_dir();
        let out = parse(
            "rivet:include generated\n\
             rivet:lib -lfoo\n\
             rivet:lib foo.lib\n\
             rivet:libdir libs\n\
             rivet:define FOO=1\n\
             rivet:define BAR\n\
             rivet:rerun-if-changed build.rivet.cpp\n\
             rivet:rerun-if-env PATH\n",
            dir.path(),
        );
        assert_eq!(out.includes, vec![dir.path().join("generated")]);
        assert_eq!(out.libs, vec!["-lfoo".to_string(), "foo.lib".to_string()]);
        assert_eq!(out.lib_dirs, vec![dir.path().join("libs")]);
        assert_eq!(out.defines, vec!["FOO=1".to_string(), "BAR".to_string()]);
        assert_eq!(
            out.rerun_if_changed,
            vec![dir.path().join("build.rivet.cpp")]
        );
        assert_eq!(out.rerun_if_env, vec!["PATH".to_string()]);
        assert!(out.log.is_empty());
    }

    #[test]
    fn parse_ignores_non_directive_lines() {
        let dir = pkg_dir();
        let out = parse(
            "hello from build script\nfoo rivet:include x\nrivet:include gen\n\ntrailing\n",
            dir.path(),
        );
        assert_eq!(out.includes, vec![dir.path().join("gen")]);
        assert_eq!(
            out.log,
            vec![
                "hello from build script".to_string(),
                "foo rivet:include x".to_string(),
                String::new(),
                "trailing".to_string(),
            ]
        );
    }

    #[test]
    fn parse_unknown_key_names_line() {
        let dir = pkg_dir();
        let err = parse_directives("ok line\nrivet:link foo\n", dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 2"), "got: {err}");
        assert!(err.contains("`link`"), "got: {err}");
        assert!(err.contains("rivet:link foo"), "got: {err}");
    }

    #[test]
    fn parse_missing_value_is_malformed() {
        let dir = pkg_dir();
        for line in [
            "rivet:include",
            "rivet:include   ",
            "rivet:define ",
            "rivet:",
        ] {
            let err = parse_directives(line, dir.path()).unwrap_err().to_string();
            assert!(err.contains(line.trim_end()), "line {line:?} got: {err}");
        }
    }

    #[test]
    fn parse_accepts_equals_separator() {
        let dir = pkg_dir();
        let out = parse("rivet:define=FOO=1\nrivet:include=gen\n", dir.path());
        assert_eq!(out.defines, vec!["FOO=1".to_string()]);
        assert_eq!(out.includes, vec![dir.path().join("gen")]);
    }

    #[test]
    fn parse_keys_are_case_sensitive() {
        let dir = pkg_dir();
        let err = parse_directives("rivet:Include gen\n", dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown directive"), "got: {err}");
        assert!(err.contains("Include"), "got: {err}");
    }

    #[test]
    fn parse_resolves_relative_paths() {
        let dir = pkg_dir();
        let abs = if cfg!(windows) {
            PathBuf::from("C:\\abs\\inc")
        } else {
            PathBuf::from("/abs/inc")
        };
        let text = format!("rivet:include {}\nrivet:libdir rel\n", abs.display());
        let out = parse(&text, dir.path());
        assert_eq!(out.includes, vec![abs]);
        assert_eq!(out.lib_dirs, vec![dir.path().join("rel")]);
    }

    #[test]
    fn parse_dedupes_repeats_keeping_order() {
        let dir = pkg_dir();
        let out = parse(
            "rivet:define A=1\nrivet:define B=2\nrivet:define A=1\nrivet:include x\nrivet:include x\n",
            dir.path(),
        );
        assert_eq!(out.defines, vec!["A=1".to_string(), "B=2".to_string()]);
        assert_eq!(out.includes, vec![dir.path().join("x")]);
    }

    #[test]
    fn parse_rejects_bad_env_and_define_values() {
        let dir = pkg_dir();
        let err = parse_directives("rivet:rerun-if-env FOO BAR\n", dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("rerun-if-env"), "got: {err}");
        let err = parse_directives("rivet:rerun-if-env FOO=1\n", dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("bare variable name"), "got: {err}");
        let err = parse_directives("rivet:define FOO BAR=1\n", dir.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("define"), "got: {err}");
    }

    #[test]
    fn script_std_enforces_minimum() {
        assert_eq!(script_std("c++11"), "c++17");
        assert_eq!(script_std("c++14"), "c++17");
        assert_eq!(script_std("c++17"), "c++17");
        assert_eq!(script_std("c++20"), "c++20");
        assert_eq!(script_std("c++23"), "c++23");
        assert_eq!(script_std("c++2a"), "c++2a");
        assert_eq!(script_std("c++latest"), "c++latest");
        assert_eq!(script_std(""), "c++17");
    }

    #[test]
    fn is_secret_var_matches_suffix() {
        assert!(is_secret_var(OsStr::new("GITHUB_TOKEN")));
        assert!(is_secret_var(OsStr::new("cargo_registry_token")));
        assert!(is_secret_var(OsStr::new("AWS_SECRET_ACCESS_KEY")));
        assert!(!is_secret_var(OsStr::new("PATH")));
        assert!(!is_secret_var(OsStr::new("TOKENIZER")));
        assert!(!is_secret_var(OsStr::new("MONKEY")));
    }

    #[test]
    fn missing_script_errors_actionably() {
        let dir = pkg_dir();
        let tc = rivet_toolchain::detect().unwrap();
        let err = compile_script(
            &tc,
            &dir.path().join(SCRIPT_FILE_NAME),
            &dir.path().join("out"),
            &CompileOptions::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no build script"), "got: {err}");
        assert!(err.contains(SCRIPT_FILE_NAME), "got: {err}");
        assert!(!has_script(dir.path()));
    }

    /// Write a fixture script; the package dir is the tempdir itself.
    fn write_fixture(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join(SCRIPT_FILE_NAME);
        std::fs::write(&path, body).unwrap();
        path
    }

    const ECHO_SCRIPT: &str = r#"
#include <cstdio>
#include <cstdlib>
int main() {
  std::printf("hello from build script\n");
  std::printf("rivet:include generated\n");
  std::printf("rivet:define FOO=1\n");
  std::printf("rivet:lib -lfoo\n");
  std::printf("rivet:libdir libs\n");
  std::printf("rivet:rerun-if-changed build.rivet.cpp\n");
  std::printf("rivet:rerun-if-env RIVET_M7_TEST_TOKEN\n");
  const char* t = std::getenv("RIVET_M7_TEST_TOKEN");
  std::printf("rivet:define SAW_TOKEN=%s\n", t ? "1" : "0");
  return 0;
}
"#;

    #[test]
    fn compile_and_run_fixture_with_real_toolchain() {
        let dir = pkg_dir();
        let tc = rivet_toolchain::detect().unwrap();
        let script = write_fixture(dir.path(), ECHO_SCRIPT);
        let out_dir = dir.path().join("script-out");
        let exe = compile_script(&tc, &script, &out_dir, &CompileOptions::default()).unwrap();
        assert!(exe.is_file());

        std::env::remove_var("RIVET_M7_TEST_TOKEN");
        let ran = run_script(&exe, dir.path(), false).unwrap();
        assert_eq!(ran.includes, vec![dir.path().join("generated")]);
        assert_eq!(ran.libs, vec!["-lfoo".to_string()]);
        assert_eq!(ran.lib_dirs, vec![dir.path().join("libs")]);
        assert!(ran.defines.contains(&"FOO=1".to_string()), "{ran:?}");
        assert!(ran.defines.contains(&"SAW_TOKEN=0".to_string()), "{ran:?}");
        assert_eq!(
            ran.rerun_if_changed,
            vec![dir.path().join(SCRIPT_FILE_NAME)]
        );
        assert_eq!(ran.rerun_if_env, vec!["RIVET_M7_TEST_TOKEN".to_string()]);
        assert_eq!(ran.log, vec!["hello from build script".to_string()]);
        assert!(!ran.script_hash.is_empty());
        assert_eq!(
            ran.rerun_file_hashes
                .get(&dir.path().join(SCRIPT_FILE_NAME)),
            Some(&Some(hash_bytes(ECHO_SCRIPT.as_bytes())))
        );

        // Freshness: identical state needs no rerun.
        assert!(!needs_rerun(dir.path(), &ran, &ran.script_hash));
        // Verbose echo works too.
        let _ = run_script(&exe, dir.path(), true).unwrap();
    }

    #[test]
    fn compile_failure_reports_diagnostics() {
        let dir = pkg_dir();
        let tc = rivet_toolchain::detect().unwrap();
        let script = write_fixture(dir.path(), "int main( { this is not C++\n");
        let err = compile_script(
            &tc,
            &script,
            &dir.path().join("out"),
            &CompileOptions::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("compile failed"), "got: {err}");
        assert!(err.contains(SCRIPT_FILE_NAME), "got: {err}");
    }

    #[test]
    fn run_nonzero_exit_reports_output() {
        let dir = pkg_dir();
        let tc = rivet_toolchain::detect().unwrap();
        let script = write_fixture(
            dir.path(),
            "#include <cstdio>\nint main() { std::printf(\"about to fail\\n\"); return 3; }\n",
        );
        let exe = compile_script(
            &tc,
            &script,
            &dir.path().join("out"),
            &CompileOptions::default(),
        )
        .unwrap();
        let err = run_script(&exe, dir.path(), false).unwrap_err().to_string();
        assert!(err.contains("exited with"), "got: {err}");
        assert!(err.contains("about to fail"), "got: {err}");
    }

    #[test]
    fn run_scrubs_secret_env() {
        let dir = pkg_dir();
        let tc = rivet_toolchain::detect().unwrap();
        let script = write_fixture(dir.path(), ECHO_SCRIPT);
        let exe = compile_script(
            &tc,
            &script,
            &dir.path().join("out"),
            &CompileOptions::default(),
        )
        .unwrap();
        std::env::set_var("RIVET_M7_TEST_TOKEN", "s3cret");
        let ran = run_script(&exe, dir.path(), false).unwrap();
        std::env::remove_var("RIVET_M7_TEST_TOKEN");
        // The script must NOT have seen the secret.
        assert!(ran.defines.contains(&"SAW_TOKEN=0".to_string()), "{ran:?}");
    }

    fn snapshotted(dir: &Path, text: &str, script_hash: &str) -> ScriptOutput {
        let mut out = parse_directives(text, dir).unwrap();
        out.script_hash = script_hash.to_string();
        let mut hashes = BTreeMap::new();
        for p in &out.rerun_if_changed {
            hashes.insert(p.clone(), hash_file_opt(p).unwrap());
        }
        out.rerun_file_hashes = hashes;
        let mut env = BTreeMap::new();
        for v in &out.rerun_if_env {
            env.insert(v.clone(), std::env::var_os(v));
        }
        out.env_values = env;
        out
    }

    #[test]
    fn needs_rerun_gates() {
        let dir = pkg_dir();
        std::fs::write(dir.path().join("watched.txt"), "v1").unwrap();
        std::env::remove_var("RIVET_M7_TEST_ENV_A");
        let prev = snapshotted(
            dir.path(),
            "rivet:rerun-if-changed watched.txt\nrivet:rerun-if-env RIVET_M7_TEST_ENV_A\n",
            "abc123",
        );
        // Identical → no rerun.
        assert!(!needs_rerun(dir.path(), &prev, "abc123"));
        // Script bytes changed → rerun.
        assert!(needs_rerun(dir.path(), &prev, "different"));
        // Watched file touched → rerun.
        std::fs::write(dir.path().join("watched.txt"), "v2").unwrap();
        assert!(needs_rerun(dir.path(), &prev, "abc123"));
        // Watched env changed → rerun.
        std::fs::write(dir.path().join("watched.txt"), "v1").unwrap();
        std::env::set_var("RIVET_M7_TEST_ENV_A", "1");
        assert!(needs_rerun(dir.path(), &prev, "abc123"));
        std::env::remove_var("RIVET_M7_TEST_ENV_A");
        // Watched file deleted → rerun.
        std::fs::remove_file(dir.path().join("watched.txt")).unwrap();
        assert!(needs_rerun(dir.path(), &prev, "abc123"));
    }

    #[test]
    fn run_package_script_none_when_absent() {
        let dir = pkg_dir();
        let tc = rivet_toolchain::detect().unwrap();
        let out = run_package_script(
            &tc,
            dir.path(),
            &dir.path().join("out"),
            &CompileOptions::default(),
            false,
        )
        .unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn run_package_script_end_to_end() {
        let dir = pkg_dir();
        let tc = rivet_toolchain::detect().unwrap();
        write_fixture(dir.path(), ECHO_SCRIPT);
        std::env::remove_var("RIVET_M7_TEST_TOKEN");
        let out = run_package_script(
            &tc,
            dir.path(),
            &dir.path().join("out"),
            &CompileOptions::default(),
            false,
        )
        .unwrap()
        .unwrap();
        assert!(out.defines.contains(&"FOO=1".to_string()));
        assert!(!needs_rerun(dir.path(), &out, &out.script_hash.clone()));
    }
}
