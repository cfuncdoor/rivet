//! Tool lookup + subprocess runner shared by all adapters.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::Error;

/// Number of output lines kept in [`Error::CommandFailed`].
const TAIL_LINES: usize = 40;

/// `true` when `tool` resolves on `PATH` (or is a directly usable path).
///
/// Implemented with a manual `PATH` scan (plus `PATHEXT` on Windows) so
/// this crate needs no lookup dependency.
pub fn tool_available(tool: &str) -> bool {
    find_tool(tool).is_some()
}

/// Find `tool`, returning an actionable [`Error::ToolMissing`] otherwise.
pub fn require_tool(tool: &str) -> Result<PathBuf, Error> {
    find_tool(tool).ok_or_else(|| Error::ToolMissing {
        tool: tool.to_string(),
        hint: install_hint(tool).to_string(),
    })
}

/// Install hint per known tool (used in [`Error::ToolMissing`]).
pub fn install_hint(tool: &str) -> &'static str {
    match tool {
        "cmake" => "install CMake 3.16+ (Windows: `winget install Kitware.CMake`; Linux: `sudo apt install cmake`; macOS: `brew install cmake`) and ensure `cmake` is on PATH",
        "meson" => "install Meson plus a backend (Linux: `sudo apt install meson ninja-build`; macOS: `brew install meson ninja`; Windows: `pip install meson` and install Ninja) and ensure `meson` is on PATH",
        "make" => "install Make (Linux: `sudo apt install make`; macOS: Xcode Command Line Tools; Windows: `winget install GnuWin32.Make` or use an MSYS2 shell — note this adapter shells out to `make` only, not `nmake`) and ensure `make` is on PATH",
        "pkg-config" => "install pkg-config (Linux: `sudo apt install pkg-config`; macOS: `brew install pkgconf`; Windows: MSYS2 `pacman -S pkgconf`) and ensure it is on PATH",
        _ => "install the tool and ensure it is on PATH",
    }
}

/// Locate `tool`: direct paths (containing a separator) are checked as-is,
/// bare names are scanned across `PATH` entries.
fn find_tool(tool: &str) -> Option<PathBuf> {
    if tool.contains('/') || tool.contains('\\') {
        return check_candidate(Path::new(tool));
    }
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        if let Some(hit) = check_candidate(&dir.join(tool)) {
            return Some(hit);
        }
    }
    None
}

/// Check one candidate path, trying `PATHEXT` suffixes on Windows when the
/// name has no extension.
fn check_candidate(base: &Path) -> Option<PathBuf> {
    if base.is_file() {
        return Some(base.to_path_buf());
    }
    #[cfg(windows)]
    {
        if base.extension().is_none() {
            let pathext = std::env::var_os("PATHEXT")
                .unwrap_or_else(|| OsStr::new(".EXE;.BAT;.CMD").to_os_string());
            // `split_paths` splits on `;` (Windows) or `:` (Unix); the inner
            // `;`-split additionally covers a `:`-joined PATHEXT on Unix.
            for segment in std::env::split_paths(&pathext) {
                for part in segment.to_string_lossy().split(';') {
                    let part = part.trim();
                    if part.is_empty() {
                        continue;
                    }
                    let cand = base.with_extension(part.trim_start_matches('.'));
                    if cand.is_file() {
                        return Some(cand);
                    }
                }
            }
        }
    }
    None
}

/// Run `cmd` to completion with output captured (never inherited, so tools
/// can never prompt mid-build). Sets `GIT_TERMINAL_PROMPT=0` so foreign
/// projects that shell out to git fail fast instead of hanging.
///
/// On success returns combined stdout+stderr; on nonzero exit (or spawn
/// failure) returns [`Error::CommandFailed`] with the command line and the
/// last [`TAIL_LINES`] lines of output.
pub(crate) fn run_capture(
    cmd: &mut Command,
    what: &str,
    hint: &str,
    extra_env: &[(&str, &str)],
) -> Result<String, Error> {
    let command = command_line(cmd);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(|e| Error::CommandFailed {
        what: what.to_string(),
        command: command.clone(),
        output: format!("<could not spawn: {e}>"),
        hint: hint.to_string(),
    })?;
    let mut combined = out.stdout;
    combined.extend_from_slice(&out.stderr);
    let text = String::from_utf8_lossy(&combined).into_owned();
    if out.status.success() {
        Ok(text)
    } else {
        Err(Error::CommandFailed {
            what: what.to_string(),
            command,
            output: tail_lines(&text, TAIL_LINES),
            hint: hint.to_string(),
        })
    }
}

/// Render `program + args` lossily for error messages.
pub(crate) fn command_line(cmd: &Command) -> String {
    let mut parts = vec![cmd.get_program().to_string_lossy().into_owned()];
    parts.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
    parts.join(" ")
}

/// Last `n` lines of `text` (or all of it when shorter).
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_tool_error_is_actionable() {
        let err = require_tool("rivet-tool-that-does-not-exist").unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("rivet-tool-that-does-not-exist"),
            "got: {text}"
        );
        assert!(text.contains("PATH"), "got: {text}");
        assert!(text.contains("Fix:"), "got: {text}");
        // Per-tool hints name the real installer.
        let cmake = require_tool("rivet-missing-cmake-xyz")
            .unwrap_err()
            .to_string();
        assert!(cmake.contains("PATH"), "got: {cmake}");
        assert!(install_hint("cmake").contains("CMake"), "hint missing");
        assert!(install_hint("meson").contains("Meson"), "hint missing");
        assert!(install_hint("pkg-config").contains("pkg"), "hint missing");
    }

    #[test]
    fn find_on_path_via_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        // Uppercase extension: matches the `PATHEXT` (`.BAT`) lookup below
        // byte-for-byte on case-preserving filesystems.
        #[cfg(windows)]
        let name = "rivet-fake-tool.BAT";
        #[cfg(not(windows))]
        let name = "rivet-fake-tool";
        let fake = dir.path().join(name);
        std::fs::write(&fake, "@echo off\n").unwrap();
        // Bare name is not on PATH, but the direct path resolves.
        assert!(find_tool("rivet-fake-tool-definitely-absent").is_none());
        assert_eq!(find_tool(&fake.to_string_lossy()).unwrap(), fake);
        // And a PATH scan finds it once its dir is prepended: exercise the
        // scan by temporarily extending PATH through a scoped child lookup.
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![dir.path().to_path_buf()];
        paths.extend(std::env::split_paths(&old_path));
        let joined = std::env::join_paths(paths).unwrap();
        let found = std::env::split_paths(&joined)
            .filter_map(|d| check_candidate(&d.join("rivet-fake-tool")))
            .next();
        assert_eq!(found.unwrap(), fake);
    }

    #[test]
    fn failing_command_reports_command_and_tail() {
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("cmd");
            c.args(["/C", "echo boom>&2 & exit /b 3"]);
            c
        };
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = Command::new("sh");
            c.args(["-c", "echo boom >&2; exit 3"]);
            c
        };
        let err = run_capture(&mut cmd, "probe test fixture", "fix the fixture", &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("probe test fixture"), "got: {err}");
        assert!(err.contains("boom"), "got: {err}");
        assert!(err.contains("fix the fixture"), "got: {err}");
    }
}
