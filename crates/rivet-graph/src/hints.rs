//! Failure hints: pattern-matched suggestions printed after compiler
//! diagnostics (`help: …`).
//!
//! Matching is hand-rolled substring logic, not regexes: the patterns are
//! few and fixed, and the workspace keeps its small dependency set. Only
//! high-confidence, rivet-specific hints fire — a wrong suggestion is
//! worse than none, so anything ambiguous yields `None`.

use std::path::PathBuf;

/// Suggest for a failed compile: `D9002`-style unknown flags first (a bad
/// flag poisons everything), then unopenable headers. At most one hint.
pub fn compile_hint(output: &str, include_dirs: &[PathBuf]) -> Option<String> {
    if let Some(opt) = unknown_option(output) {
        return Some(format!(
            "compiler does not know flag `{opt}` — it predates your toolchain; use an older `standard` (or `c++latest`) in rivet.json"
        ));
    }
    if let Some(header) = missing_header(output) {
        return Some(missing_header_hint(&header, include_dirs));
    }
    None
}

/// Suggest for a failed link (CRT mismatches; nothing else is confident).
pub fn link_hint(output: &str) -> Option<String> {
    if output.contains("LNK2038")
        && (output.contains("_ITERATOR_DEBUG_LEVEL") || output.contains("RuntimeLibrary"))
    {
        return Some(
            "C-runtime mismatch between objects: rivet builds with the driver-default static CRT (/MT) while a foreign archive used another (e.g. CMake's /MDd) — set the dep adapter's CMAKE_MSVC_RUNTIME_LIBRARY (or meson b_vscrt) to match, then rebuild the dep"
                .to_string(),
        );
    }
    None
}

/// An unknown compiler flag, if any: MSVC `D9002`, GCC
/// `unrecognized command-line option`, Clang `invalid value … -std=`.
fn unknown_option(output: &str) -> Option<String> {
    for line in output.lines() {
        if line.contains("D9002") && line.contains("ignoring unknown option") {
            if let Some(opt) = quoted_after(line, "ignoring unknown option ") {
                return Some(opt);
            }
            return Some("unknown option".to_string());
        }
        if line.contains("unrecognized command-line option") {
            if let Some(opt) = quoted_after(line, "unrecognized command-line option ") {
                return Some(opt);
            }
            return Some("unknown option".to_string());
        }
        if line.contains("invalid value") && line.contains("-std=") {
            return Some("-std= value".to_string());
        }
    }
    None
}

/// Text between the first pair of single quotes at/after `marker`.
/// `marker` itself may end with the opening quote.
fn quoted_after(line: &str, marker: &str) -> Option<String> {
    let at = line.find(marker)?;
    let rest = &line[at + marker.len()..];
    let rest = rest.strip_prefix('\'').unwrap_or(rest);
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

/// A header the compiler could not open, if any: MSVC `C1083` or GCC/Clang
/// `fatal error: …: No such file or directory`.
fn missing_header(output: &str) -> Option<String> {
    for line in output.lines() {
        if line.contains("C1083") && line.contains("Cannot open include file:") {
            if let Some(header) = quoted_after(line, "Cannot open include file: ") {
                return Some(header);
            }
        }
        if line.contains("fatal error:") && line.contains("No such file or directory") {
            if let Some(after) = line.split("fatal error:").nth(1) {
                let header = after.split(": No such file or directory").next()?;
                let header = header.trim().to_string();
                if !header.is_empty() {
                    return Some(header);
                }
            }
        }
    }
    None
}

/// Explain a missing `header` against the include dirs that were passed.
/// Checks the first path segment: when it names an existing sibling dir
/// whose file is absent, the filename (not the search path) is wrong —
// the single most common shape of this error with vendored deps.
fn missing_header_hint(header: &str, include_dirs: &[PathBuf]) -> String {
    let first = header.split(['/', '\\']).next().unwrap_or(header);
    if !first.is_empty() && first != header {
        let mut holders = Vec::new();
        for dir in include_dirs {
            if dir.join(first).is_dir() {
                holders.push(dir.clone());
            }
        }
        if let Some(dir) = holders.into_iter().next() {
            return format!(
                "header `{header}` was not found, but `{}` exists — the file is likely named or placed differently (check the dep's layout; `sources`-adapter deps expose only the tree root, subdirectories are ignored)",
                dir.join(first).display()
            );
        }
    }
    format!(
        "header `{header}` is not provided by any include dir on the command line — check the spelling; if it belongs to a dep, the dep may not expose that dir (or failed to build); `-v` shows the full command line including every include dir"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn d9002_flag_detected() {
        let out = "cl : Command line warning D9002 : ignoring unknown option '/std:c++23'\n";
        assert_eq!(
            compile_hint(out, &[]),
            Some("compiler does not know flag `/std:c++23` — it predates your toolchain; use an older `standard` (or `c++latest`) in rivet.json".to_string())
        );
        let out = "g++: error: unrecognized command-line option '--std=c++2b'\n";
        assert!(compile_hint(out, &[]).is_some_and(|h| h.contains("--std=c++2b")));
        let out = "error: invalid value 'c++23' in '-std=c++23'\n";
        assert!(compile_hint(out, &[]).is_some_and(|h| h.contains("standard")));
    }

    #[test]
    fn c1083_sibling_dir_detected() {
        let home = dirs();
        let inc = home.path().join("include");
        std::fs::create_dir_all(inc.join("safetyhook")).unwrap();
        std::fs::write(inc.join("safetyhook.hpp"), b"x").unwrap();
        let err = "main.cpp(3): fatal error C1083: Cannot open include file: 'safetyhook/safetyhook.hpp': No such file or directory\n";
        let hint = compile_hint(err, std::slice::from_ref(&inc)).unwrap();
        assert!(hint.contains("safetyhook.hpp"), "got: {hint}");
        assert!(
            hint.contains(&inc.join("safetyhook").display().to_string()),
            "got: {hint}"
        );
    }

    #[test]
    fn c1083_without_holder_is_generic() {
        let err = "fatal error C1083: Cannot open include file: 'nope/missing.h': No such file or directory\n";
        let hint = compile_hint(err, &[PathBuf::from("/empty")]).unwrap();
        assert!(hint.contains("check the spelling"), "got: {hint}");
        assert!(hint.contains("nope/missing.h"), "got: {hint}");
    }

    #[test]
    fn gcc_missing_header_detected() {
        let err = "main.cpp:3:10: fatal error: nope/missing.h: No such file or directory\n";
        assert!(compile_hint(err, &[]).is_some_and(|h| h.contains("nope/missing.h")));
    }

    #[test]
    fn clean_output_has_no_hint() {
        assert_eq!(compile_hint("main.cpp\n", &[]), None);
        assert_eq!(compile_hint("", &[]), None);
        assert_eq!(link_hint(""), None);
    }

    #[test]
    fn lnk2038_detected() {
        let out = "foo.obj : error LNK2038: mismatch detected for 'RuntimeLibrary': value 'MDd_DynamicDebug' doesn't match value 'MT_StaticRelease' in bar.obj\n";
        let hint = link_hint(out).unwrap();
        assert!(hint.contains("CMAKE_MSVC_RUNTIME_LIBRARY"), "got: {hint}");
        assert_eq!(link_hint("unresolved external symbol _foo\n"), None);
    }
}
