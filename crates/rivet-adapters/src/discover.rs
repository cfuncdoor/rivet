//! Discovery of adapter-built outputs: library files + include dirs.
//!
//! The integrator feeds these into `include-dirs` (compile path) and
//! lib-files/link inputs exactly like `rivet-graph` `unit_inputs`.

use std::path::{Path, PathBuf};

use super::{Error, LibLink};

/// Header extensions recognized when scanning for include dirs.
const HEADER_EXTS: &[&str] = &["h", "hpp", "hh", "hxx"];

/// Subtrees never scanned for generated headers (tool internals, not
/// public or generated-public headers).
const SKIPPED_DIRS: &[&str] = &["CMakeFiles"];

/// Scan `build_dir` (recursively) for produced libraries, keeping only
/// files matching `targets` (empty = every discovered library).
///
/// Returns the matched files as [`LibLink::File`] in link order: static
/// archives first, then Windows import libs (a `.lib` with a same-stem
/// `.dll` next door), then shared objects. Bare `.dll` files are never
/// returned (MSVC cannot link them — LNK1107; the import `.lib` covers
/// them, and their dirs still surface via [`dll_dirs`] for runtime
/// lookup). An empty `targets` list accepts everything; a non-empty list
/// that matches nothing is [`Error::NoLibraries`] — never a silent "build
/// everything instead".
pub fn discover_libs(build_dir: &Path, targets: &[String]) -> Result<Vec<LibLink>, Error> {
    let mut files = Vec::new();
    collect_lib_files(build_dir, &mut files);
    files.sort();
    files.dedup();
    // Bare DLLs are runtime-only (see above); drop them before matching so
    // a `targets` filter naming a DLL stem matches its import lib instead.
    files.retain(|f| !is_bare_dll(f));
    let matched: Vec<PathBuf> = if targets.is_empty() {
        files
    } else {
        files
            .into_iter()
            .filter(|f| {
                targets
                    .iter()
                    .any(|t| file_name(f).is_some_and(|n| stem_matches_target(n, t)))
            })
            .collect()
    };
    if matched.is_empty() {
        let mut found = Vec::new();
        let mut all = Vec::new();
        collect_lib_files(build_dir, &mut all);
        all.sort();
        for f in &all {
            found.push(file_name(f).unwrap_or("<unprintable>").to_string());
        }
        return Err(Error::NoLibraries {
            dir: build_dir.display().to_string(),
            targets: if targets.is_empty() {
                "<default>".to_string()
            } else {
                targets.join(", ")
            },
            found: if found.is_empty() {
                "<none>".to_string()
            } else {
                found.join(", ")
            },
        });
    }
    let mut ranked: Vec<(u8, PathBuf)> = matched
        .into_iter()
        .map(|f| {
            let rank = link_rank(&f);
            (rank, f)
        })
        .collect();
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    Ok(ranked.into_iter().map(|(_, f)| LibLink::File(f)).collect())
}

/// Parent dirs of [`LibLink::File`] entries, deduped and sorted.
/// [`LibLink::Name`] entries contribute nothing.
pub fn lib_dirs_of(libs: &[LibLink]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = libs
        .iter()
        .filter_map(|l| match l {
            LibLink::File(p) => p.parent().map(Path::to_path_buf),
            LibLink::Name(_) => None,
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Dirs holding `.dll` files under `build_dir` (runtime lookup), sorted and
/// deduped. Bare DLLs never link (see [`discover_libs`]) but their
/// directories still belong on the runtime search path.
pub fn dll_dirs(build_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    collect_dll_dirs(build_dir, &mut dirs);
    dirs.sort();
    dirs.dedup();
    dirs
}

/// `true` for a `.dll` file (link-excluded; see [`discover_libs`]).
fn is_bare_dll(path: &Path) -> bool {
    file_name(path)
        .map(|n| n.to_ascii_lowercase().ends_with(".dll"))
        .unwrap_or(false)
}

fn collect_dll_dirs(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() || !ft.is_dir() && !ft.is_file() {
            continue;
        }
        if ft.is_dir() {
            collect_dll_dirs(&path, out);
        } else if is_bare_dll(&path) {
            if let Some(parent) = path.parent() {
                out.push(parent.to_path_buf());
            }
        }
    }
}

/// Public include dirs for a source-built dependency:
///
/// - `<source>/include` when present (the `target_include_directories`
///   convention),
/// - `<source>` itself when it carries top-level headers (e.g. zlib's
///   `zlib.h` at the repo root),
/// - `<build>` when the build tree holds headers outside tool-internal
///   dirs (generated config headers such as zlib's `zconf.h`).
///
/// Order is significant (source first); every entry exists.
pub fn discover_include_dirs(source_dir: &Path, build_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let src_include = source_dir.join("include");
    if src_include.is_dir() {
        dirs.push(src_include);
    }
    if has_top_level_headers(source_dir) && !dirs.iter().any(|d: &PathBuf| d == source_dir) {
        dirs.push(source_dir.to_path_buf());
    }
    if build_dir != source_dir && tree_has_headers(build_dir) {
        dirs.push(build_dir.to_path_buf());
    }
    dirs
}

// --- library files ---------------------------------------------------------

/// Recursively collect library-looking files (no dir-symlink following;
/// absolute paths are anchored at `build_dir` exactly as passed).
fn collect_lib_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            // Versioned `.so` symlinks (`libfoo.so -> libfoo.so.1`) must
            // not double-report, and dir symlinks must not escape the tree.
            // The real files are found by the direct scan.
            continue;
        }
        if ft.is_dir() {
            collect_lib_files(&path, out);
        } else if ft.is_file() && file_name(&path).is_some_and(is_lib_file_name) {
            out.push(path);
        }
    }
}

/// `true` for archive/shared-lib file names, including versioned
/// `libfoo.so.1.2`.
fn is_lib_file_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".lib")
        || lower.ends_with(".a")
        || lower.ends_with(".so")
        || lower.ends_with(".dll")
        || lower.ends_with(".dylib")
        || lower.contains(".so.")
}

/// `true` when `file_name` (a library file name) names adapter `target`:
/// `lib` prefix, extensions, and `.so` version suffixes are stripped from
/// both sides and the comparison is ASCII case-insensitive. An exact
/// (case-insensitive) filename match also counts, so `make` targets that
/// name the archive file (`libmini.a`) match the built file byte-for-byte.
/// A bare `target` (`mini`, `ma`) matches `libmini.a`/`libma.a` via the
/// normalized stem, and a target carrying a relative path (`sub/libfoo.a`)
/// matches on its file-name portion.
fn stem_matches_target(file_name: &str, target: &str) -> bool {
    let file_lower = file_name.to_ascii_lowercase();
    let mut target_lower = target.to_ascii_lowercase();
    // A make target may carry a relative path; only its file name matters.
    if let Some(pos) = target_lower.rfind(['/', '\\']) {
        target_lower = target_lower[pos + 1..].to_string();
    }
    if file_lower == target_lower {
        return true;
    }
    normalize_lib_stem(file_name) == normalize_lib_stem(&target_lower)
}

/// Strip version suffix, extensions, and a leading `lib`.
fn normalize_lib_stem(file_name: &str) -> String {
    let mut lower = file_name.to_ascii_lowercase();
    if let Some(pos) = lower.find(".so.") {
        lower.truncate(pos + 3);
    }
    for ext in [".dylib", ".lib", ".dll", ".so", ".a"] {
        if let Some(stripped) = lower.strip_suffix(ext) {
            lower = stripped.to_string();
            break;
        }
    }
    // A `.so.1`-style remainder after truncation cannot occur (handled
    // above); strip a second extension for `foo.lib`-in-`.a` oddities is
    // unnecessary — one pass matches every produced artifact.
    lower.strip_prefix("lib").unwrap_or(&lower).to_string()
}

/// Link order rank: pure static archives first, then import libs, then
/// shared objects, preferring static.
fn link_rank(path: &Path) -> u8 {
    let name = file_name(path).unwrap_or_default().to_ascii_lowercase();
    if name.ends_with(".a") {
        return 0;
    }
    if name.ends_with(".lib") {
        return if has_sibling_dll(path) { 1 } else { 0 };
    }
    2
}

/// `true` when a same-stem `.dll` sits next to `path` (making `path` an
/// import lib rather than a static archive).
fn has_sibling_dll(path: &Path) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    let stem = normalize_lib_stem(file_name(path).unwrap_or_default());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        let n = e.file_name().to_string_lossy().to_ascii_lowercase();
        n.ends_with(".dll") && normalize_lib_stem(&n) == stem
    })
}

fn file_name(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

// --- include dirs ----------------------------------------------------------

/// `true` when `dir` holds at least one header file directly.
fn has_top_level_headers(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_type().is_ok_and(|t| t.is_file())
            && e.path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| HEADER_EXTS.contains(&x.to_ascii_lowercase().as_str()))
    })
}

/// `true` when the tree holds any header outside [`SKIPPED_DIRS`].
fn tree_has_headers(dir: &Path) -> bool {
    fn walk(dir: &Path) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            let path = entry.path();
            if ft.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if SKIPPED_DIRS.contains(&name.as_str()) {
                    continue;
                }
                if walk(&path) {
                    return true;
                }
            } else if ft.is_file()
                && path
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| HEADER_EXTS.contains(&x.to_ascii_lowercase().as_str()))
            {
                return true;
            }
        }
        false
    }
    walk(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn static_archives_sort_before_shared() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("libfoo.so"));
        touch(&dir.path().join("libfoo.a"));
        touch(&dir.path().join("libbar.so.1.2.3"));
        let libs = discover_libs(dir.path(), &[]).unwrap();
        let names: Vec<String> = libs.iter().map(|l| l.to_string()).collect();
        assert_eq!(libs.len(), 3, "got: {names:?}");
        // Static first, then versioned shared (name-sorted within rank).
        assert!(names[0].ends_with("libfoo.a"), "got: {names:?}");
    }

    #[test]
    fn windows_import_lib_ranks_between_static_and_shared() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("foo.dll"));
        touch(&dir.path().join("foo.lib")); // import lib (sibling dll)
        touch(&dir.path().join("solo.lib")); // pure static
        let libs = discover_libs(dir.path(), &[]).unwrap();
        let names: Vec<String> = libs
            .iter()
            .map(|l| {
                Path::new(&l.to_string())
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        // Bare DLLs never link on MSVC (LNK1107): only the import lib is
        // returned; the DLL's dir still surfaces for runtime lookup.
        assert_eq!(
            names,
            vec!["solo.lib".to_string(), "foo.lib".to_string()],
            "got: {names:?}"
        );
        assert_eq!(dll_dirs(dir.path()).len(), 1);
    }

    #[test]
    fn bare_dll_without_import_lib_is_excluded() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("only.dll"));
        // Nothing linkable: surfaces as NoLibraries (with the dir named),
        // not as a corrupt link line.
        let err = discover_libs(dir.path(), &[]).unwrap_err().to_string();
        assert!(
            err.contains(dir.path().to_string_lossy().as_ref()),
            "got: {err}"
        );
        // Its directory is still reported for runtime lookup.
        assert_eq!(dll_dirs(dir.path()).len(), 1);
    }

    #[test]
    fn target_filtering_matches_stems_lib_aware() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Debug").join("ta.lib"));
        touch(&dir.path().join("Debug").join("tb.lib"));
        let libs = discover_libs(dir.path(), &["ta".to_string()]).unwrap();
        assert_eq!(libs.len(), 1);
        assert!(libs[0].to_string().ends_with("ta.lib"));
        // Case-insensitive, `lib` prefix transparent.
        let libs = discover_libs(dir.path(), &["TB".to_string()]).unwrap();
        assert_eq!(libs.len(), 1);
    }

    #[test]
    fn unmatched_targets_are_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("ta.lib"));
        let err = discover_libs(dir.path(), &["nope".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"), "got: {err}");
        assert!(err.contains("ta.lib"), "got: {err}");
        // And an empty build dir errors too.
        let empty = tempfile::tempdir().unwrap();
        let err = discover_libs(empty.path(), &[]).unwrap_err().to_string();
        assert!(err.contains("<none>"), "got: {err}");
        assert!(err.contains("<default>"), "got: {err}");
    }

    #[test]
    fn lib_dirs_come_from_file_parents() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("Debug").join("ta.lib");
        let libs = vec![LibLink::File(a.clone()), LibLink::Name("m".to_string())];
        let dirs = lib_dirs_of(&libs);
        assert_eq!(dirs, vec![a.parent().unwrap().to_path_buf()]);
    }

    #[test]
    fn include_dir_heuristics() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let build = dir.path().join("build");
        std::fs::create_dir_all(src.join("include").join("mini")).unwrap();
        std::fs::create_dir_all(&build).unwrap();
        // Build tree without headers: not added.
        let found = discover_include_dirs(&src, &build);
        assert_eq!(found, vec![src.join("include")], "got: {found:?}");
        // Top-level source header: source root added.
        std::fs::write(src.join("zlib.h"), b"x").unwrap();
        // Generated header in build tree: build root added; headers under
        // CMakeFiles do not count.
        std::fs::create_dir_all(build.join("CMakeFiles")).unwrap();
        std::fs::write(build.join("CMakeFiles").join("internal.h"), b"x").unwrap();
        std::fs::write(build.join("zconf.h"), b"x").unwrap();
        let found = discover_include_dirs(&src, &build);
        assert_eq!(
            found,
            vec![src.join("include"), src.clone(), build.clone()],
            "got: {found:?}"
        );
    }

    #[test]
    fn stem_normalization() {
        assert_eq!(normalize_lib_stem("libfoo.a"), "foo");
        assert_eq!(normalize_lib_stem("foo.lib"), "foo");
        assert_eq!(normalize_lib_stem("libfoo.so.1.2"), "foo");
        assert_eq!(normalize_lib_stem("FOO.DLL"), "foo");
        assert_eq!(normalize_lib_stem("libbar.dylib"), "bar");
        assert!(stem_matches_target("libzlibstatic.a", "zlibstatic"));
        assert!(stem_matches_target("zlibstatic.lib", "ZLIBSTATIC"));
        // Make-style targets name the archive file itself.
        assert!(stem_matches_target("libmini.a", "libmini.a"));
        assert!(stem_matches_target("libmini.a", "mini"));
        assert!(stem_matches_target("libmini.a", "LIBMINI.A"));
        assert!(stem_matches_target("libfoo.so.1.2", "libfoo.so.1.2"));
        assert!(stem_matches_target("foo.lib", "sub/dir/foo.lib"));
    }
}
