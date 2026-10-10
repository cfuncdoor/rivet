//! Header-dependency extraction.
//!
//! Two mechanisms, chosen by [`rivet_toolchain::CxxToolchain::dep_style`]:
//!
//! - GCC/Clang write a Makefile-style depfile (`-MMD -MF`). [`parse_gcc_depfile`]
//!   reads prerequisites after the first `target:` separator. The split point
//!   is the first colon followed by whitespace, so Windows drive-letter colons
//!   (`C:/…`) are safe; backslash escapes (`\ `, `\\`) and line continuations
//!   are honored.
//! - MSVC-like compilers print `Note: including file: <path>` lines on stdout
//!   under `/showIncludes` ([`parse_show_includes`]). The prefix is
//!   locale-dependent, so rivet forces `VSLANG=1033` on every spawned command.

use std::path::{Path, PathBuf};

/// Parse a GCC/Clang depfile, returning prerequisite paths in order.
///
/// `pkg_dir` anchors relative entries (depfile paths are relative to the
/// compiler's working directory, which rivet always sets to the package
/// dir). Results are lexically normalized but not canonicalized (no
/// symlink resolution, no existence check — a deleted header must still
/// compare as "changed", not vanish).
pub fn parse_gcc_depfile(text: &str, pkg_dir: &Path) -> Vec<PathBuf> {
    // Split target from prerequisites at the first colon followed by
    // whitespace. Drive-letter colons (`C:/x`) are never followed by space.
    let mut prereqs = text;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b':' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_whitespace() {
            prereqs = &text[i + 1..];
            break;
        }
        i += 1;
    }
    // Tokenize: `\` + newline = continuation, `\` + c = literal c.
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has_token = false;
    let mut chars = prereqs.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('\n') => {
                    chars.next();
                    continue;
                }
                Some('\r') => {
                    chars.next();
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    continue;
                }
                Some(&next) => {
                    cur.push(next);
                    has_token = true;
                    chars.next();
                    continue;
                }
                None => break,
            }
        }
        if c.is_whitespace() {
            if has_token {
                out.push(join_pkg(pkg_dir, &cur));
                cur.clear();
                has_token = false;
            }
            continue;
        }
        cur.push(c);
        has_token = true;
    }
    if has_token {
        out.push(join_pkg(pkg_dir, &cur));
    }
    out.sort();
    out.dedup();
    out
}

/// Parse `/showIncludes` stdout, returning included paths in order.
/// Only lines starting with the English prefix are notes; everything else
/// (the echoed TU name, warnings) is ignored.
pub fn parse_show_includes(stdout: &str, pkg_dir: &Path) -> Vec<PathBuf> {
    const PREFIX: &str = "Note: including file:";
    let mut out: Vec<PathBuf> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix(PREFIX))
        .map(|rest| join_pkg(pkg_dir, rest.trim()))
        .collect();
    out.sort();
    out.dedup();
    out
}

fn join_pkg(pkg_dir: &Path, raw: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    let abs = if p.is_absolute() { p } else { pkg_dir.join(p) };
    normalize(&abs)
}

/// Strip `/showIncludes` note lines for display: they are header-tracking
/// metadata rivet already consumed, not diagnostics. Everything else
/// passes through byte-identical (no UTF-8 conversion, newlines preserved).
pub fn strip_note_lines(bytes: &[u8]) -> Vec<u8> {
    const PREFIX: &[u8] = b"Note: including file:";
    let mut out = Vec::with_capacity(bytes.len());
    for line in bytes.split_inclusive(|&b| b == b'\n') {
        if !line.starts_with(PREFIX) {
            out.extend_from_slice(line);
        }
    }
    out
}

/// Drop a leading bare TU-echo line (`main.cpp`): cl prints the file it
/// starts on, but rivet's summary error already names the TU. Drops only
/// the first line, and only when it exactly names the TU (bare filename,
/// relative, or absolute path); anything else passes through byte-identical.
pub fn strip_tu_echo(bytes: &[u8], echo_names: &[&str]) -> Vec<u8> {
    let mut parts = bytes.splitn(2, |&b| b == b'\n');
    let first = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default();
    let trimmed = trim_ascii(first);
    if !trimmed.is_empty() && echo_names.iter().any(|n| trimmed == n.as_bytes()) {
        return rest.to_vec();
    }
    bytes.to_vec()
}

/// Strip ASCII whitespace at both ends (byte-level; `trim_ascii` needs a
/// newer Rust than the MSRV).
fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.first(), Some(b) if b.is_ascii_whitespace()) {
        bytes = &bytes[1..];
    }
    while matches!(bytes.last(), Some(b) if b.is_ascii_whitespace()) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

/// Lexically normalize a path (resolve `.`/`..` without touching the fs).
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcc_basic_and_continuations() {
        let text = "build/a.o: src/a.cpp src/a.h \\\n  include/x.h\n";
        let deps = parse_gcc_depfile(text, Path::new("/pkg"));
        assert_eq!(
            deps,
            vec![
                PathBuf::from("/pkg/include/x.h"),
                PathBuf::from("/pkg/src/a.cpp"),
                PathBuf::from("/pkg/src/a.h"),
            ],
            "got: {deps:?}"
        );
    }

    #[test]
    fn gcc_windows_drive_colons_survive() {
        let text = "C:/b/a.o: C:/pkg/src/a.cpp C:/pkg/inc/a.h\n";
        let deps = parse_gcc_depfile(text, Path::new("C:/pkg"));
        assert_eq!(deps.len(), 2, "got: {deps:?}");
        assert!(
            deps[0].to_string_lossy().contains("a.cpp")
                || deps[1].to_string_lossy().contains("a.cpp")
        );
    }

    #[test]
    fn gcc_escaped_spaces() {
        let text = "o: src/a.cpp /x/My\\ Dir/b.h\n";
        let deps = parse_gcc_depfile(text, Path::new("/pkg"));
        assert_eq!(deps.len(), 2, "got: {deps:?}");
        assert!(
            deps.iter().any(|p| p.to_string_lossy().contains("My Dir")),
            "got: {deps:?}"
        );
    }

    #[test]
    fn gcc_dotdot_normalizes() {
        let text = "o: src/../include/a.h\n";
        let deps = parse_gcc_depfile(text, Path::new("/pkg"));
        assert_eq!(deps, vec![PathBuf::from("/pkg/include/a.h")]);
    }

    #[test]
    fn show_includes_parses_notes_only() {
        let out = "main.cpp\nNote: including file: C:\\SDK\\iostream\nNote: including file: D:\\p\\src\\a.h\nsome warning\n";
        let deps = parse_show_includes(out, Path::new("D:/p"));
        assert_eq!(deps.len(), 2, "got: {deps:?}");
    }

    #[test]
    fn show_includes_relative_resolves() {
        let out = "Note: including file: src/a.h\n";
        let deps = parse_show_includes(out, Path::new("/pkg"));
        assert_eq!(deps, vec![PathBuf::from("/pkg/src/a.h")]);
    }

    #[test]
    fn strip_note_lines_keeps_diagnostics_byte_identical() {
        let out = b"main.cpp\nNote: including file: C:\\SDK\\iostream\nsome warning C4000: x\nNote: including file: D:\\a.h\n";
        let stripped = strip_note_lines(out);
        assert_eq!(&stripped, b"main.cpp\nsome warning C4000: x\n");
        // No trailing newline: preserved as-is, notes still gone.
        let out = b"main.cpp\nNote: including file: C:\\SDK\\iostream";
        assert_eq!(&strip_note_lines(out), b"main.cpp\n");
        // All notes: empty output (caller prints nothing then).
        let out = b"Note: including file: a.h\n";
        assert!(strip_note_lines(out).is_empty());
        // A near-miss prefix is not a note.
        let out = b"Note: including files: x\n";
        assert_eq!(&strip_note_lines(out), out);
    }

    #[test]
    fn strip_tu_echo_drops_only_the_first_line_when_named() {
        let names = ["main.cpp", "src/main.cpp", "D:/p/src/main.cpp"];
        // Bare echo first: gone, rest byte-identical.
        let out = b"main.cpp\nmain.cpp(3): error C1083: x\n";
        assert_eq!(
            &strip_tu_echo(out, &names),
            b"main.cpp(3): error C1083: x\n"
        );
        // Absolute echo: gone too.
        let out = b"D:/p/src/main.cpp\nerror C2059\n";
        assert_eq!(&strip_tu_echo(out, &names), b"error C2059\n");
        // Unknown first line: kept.
        let out = b"other.cpp\nerror C2059\n";
        assert_eq!(&strip_tu_echo(out, &names), out);
        // Echo later than first: kept (only the head echo is rivet's to drop).
        let out = b"error C2059\nmain.cpp\n";
        assert_eq!(&strip_tu_echo(out, &names), out);
        // Empty and whitespace-only: kept.
        assert_eq!(&strip_tu_echo(b"", &names), b"");
        assert_eq!(&strip_tu_echo(b"\n", &names), b"\n");
    }
}
