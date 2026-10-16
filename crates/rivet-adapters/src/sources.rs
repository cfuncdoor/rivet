//! `sources` adapter: compile schemaless foreign sources directly.
//!
//! For foreign trees with C/C++ sources but no build system (no
//! `rivet.json`, no `CMakeLists.txt`/`meson.build`/`Makefile`): compiles
//! the **top-level** sources with the rivet toolchain into one static
//! archive and exposes the source root (plus `include/` when present)
//! as include dirs. Subdirectories are never descended — nested trees
//! (e.g. imgui's `backends/`) need platform wiring the adapter cannot
//! guess, so they are ignored by design.
//!
//! Only the local C/C++ compiler runs here; no foreign code executes at
//! build time (same guarantee as the other source-building adapters).
//! Rebuilds are gated by the standard adapter fingerprint (whole-tree
//! hash + spec + toolchain), so no dep tracking is needed: any source
//! or header change rebuilds the archive wholesale.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rivet_manifest::AdapterSpec;
use rivet_toolchain::{CompileOptions, CxxToolchain};

use super::util::run_capture;
use super::LibLink;
use super::{AdapterCtx, AdapterOutput, Error};

/// Source extensions compiled by this adapter (matched case-insensitively;
/// drivers pick C vs C++ by extension as usual).
pub const SOURCE_EXTS: &[&str] = &["cpp", "cc", "cxx", "c"];

/// Sorted top-level C/C++ sources in `source_dir` (files only; subdirs are
/// never descended). Returns an empty vec when there is nothing to build
/// (the caller turns that into [`Error::NoLibraries`]).
pub fn source_files(source_dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let entries = std::fs::read_dir(source_dir)?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let is_source = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| SOURCE_EXTS.contains(&e.to_ascii_lowercase().as_str()));
        if is_source {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Foreign layout of a materialized checkout, by top-level files only.
/// Centralizes the "does this tree need an adapter, and which" decision
/// shared by `rivet add` and documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignLayout {
    /// Own `rivet.json`: plain git/path entry, nothing to generate.
    Manifest,
    /// `CMakeLists.txt`: `adapter.tool = "cmake"`.
    Cmake,
    /// `meson.build`: `adapter.tool = "meson"`.
    Meson,
    /// `Makefile`/`makefile`/`GNUmakefile`: `adapter.tool = "make"`.
    Make,
    /// No manifest and no recognized build, but compilable top-level
    /// sources: `adapter.tool = "sources"`.
    BareSources,
    /// No manifest, no build, no sources: resolves and builds vacuously.
    BareEmpty,
}

/// Adapter tool for a layout (`None` for manifest-carrying or empty trees).
pub fn adapter_for_layout(layout: ForeignLayout) -> Option<&'static str> {
    match layout {
        ForeignLayout::Manifest | ForeignLayout::BareEmpty => None,
        ForeignLayout::Cmake => Some("cmake"),
        ForeignLayout::Meson => Some("meson"),
        ForeignLayout::Make => Some("make"),
        ForeignLayout::BareSources => Some("sources"),
    }
}

/// Classify `dir` by top-level files. IO errors propagate (an unreadable
/// checkout must fail loudly, not guess).
pub fn detect_layout(dir: &Path) -> Result<ForeignLayout, Error> {
    if dir.join("rivet.json").is_file() {
        return Ok(ForeignLayout::Manifest);
    }
    if dir.join("CMakeLists.txt").is_file() {
        return Ok(ForeignLayout::Cmake);
    }
    if dir.join("meson.build").is_file() {
        return Ok(ForeignLayout::Meson);
    }
    if ["Makefile", "makefile", "GNUmakefile"]
        .iter()
        .any(|f| dir.join(f).is_file())
    {
        return Ok(ForeignLayout::Make);
    }
    if source_files(dir)?.is_empty() {
        return Ok(ForeignLayout::BareEmpty);
    }
    Ok(ForeignLayout::BareSources)
}

/// Compile the top-level sources per `spec` into a static archive under
/// `ctx.build_dir`.
///
/// `spec.targets` names the archive (at most one entry; default
/// `foreign`); `spec.options` must be empty (there is nothing tool-
/// specific to configure — extra defines belong in a real manifest).
/// Flags follow `ctx.build_type` like rivet's own profiles (`Debug` →
/// unoptimized with debug info, `Release` → `-O3`-class without).
pub fn build(
    source_dir: &Path,
    spec: &AdapterSpec,
    ctx: &AdapterCtx,
) -> Result<AdapterOutput, Error> {
    if spec.targets.len() > 1 {
        return Err(Error::InvalidSpec {
            message: format!(
                "sources adapter takes at most one `targets` entry (the archive name), got {}. Fix: leave `targets` empty for the default name or keep exactly one",
                spec.targets.len()
            ),
        });
    }
    if !spec.options.is_empty() {
        return Err(Error::InvalidSpec {
            message: "sources adapter takes no `options` (there is nothing tool-specific to configure). Fix: remove `options`, or move extra defines into a rivet.json manifest".to_string(),
        });
    }
    let tc = ctx.toolchain.clone().ok_or_else(|| Error::InvalidSpec {
        message: "sources adapter needs a C/C++ toolchain in `AdapterCtx`. Fix: attach one with `AdapterCtx::with_toolchain`".to_string(),
    })?;
    let sources = source_files(source_dir)?;
    if sources.is_empty() {
        return Err(Error::NoLibraries {
            dir: source_dir.display().to_string(),
            targets: spec
                .targets
                .first()
                .cloned()
                .unwrap_or("<default>".to_string()),
            found: top_level_names(source_dir),
        });
    }
    let release = ctx.build_type.eq_ignore_ascii_case("release");
    let mut include_dirs = vec![source_dir.to_path_buf()];
    if source_dir.join("include").is_dir() {
        include_dirs.push(source_dir.join("include"));
    }
    let opts = CompileOptions {
        opt: if release { 3 } else { 0 },
        debug: !release,
        include_dirs: include_dirs.clone(),
        ..CompileOptions::default()
    };

    let objs_dir = ctx.build_dir.join("objs");
    std::fs::create_dir_all(&objs_dir)?;
    std::fs::create_dir_all(&ctx.build_dir)?;
    let objects = compile_all(&tc, &sources, &objs_dir, &opts, ctx.jobs)?;

    let lib_name = spec
        .targets
        .first()
        .map(String::as_str)
        .unwrap_or("foreign");
    let archive = ctx.build_dir.join(tc.staticlib_filename(lib_name));
    let mut archive_cmd =
        tc.archive_command(&objects, &archive)
            .map_err(|e| Error::InvalidSpec {
                message: format!("cannot archive {}: {e}", archive.display()),
            })?;
    run_capture(
        &mut archive_cmd,
        &format!("archive {} objects", objects.len()),
        "the archiver failed on valid objects; check disk space and retry",
        &[],
    )?;

    Ok(AdapterOutput {
        include_dirs,
        lib_dirs: vec![ctx.build_dir.clone()],
        libs: vec![LibLink::File(archive)],
        defines: Vec::new(),
    })
}

/// Top-level entry names (capped) for [`Error::NoLibraries`].
fn top_level_names(dir: &Path) -> String {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    if names.is_empty() {
        return "<none>".to_string();
    }
    const CAP: usize = 8;
    let shown = names[..names.len().min(CAP)].join(", ");
    if names.len() > CAP {
        format!("{shown}, ({} more)", names.len() - CAP)
    } else {
        shown
    }
}

/// Object file for `src`, disambiguating stem collisions (`a.cpp` +
/// `a.cc` → `a_cc.o`) so no two TUs share an object path. Stems fold
/// case on Windows, where `A.obj` and `a.obj` are one file.
fn object_for(
    objs_dir: &Path,
    src: &Path,
    ext: &str,
    taken: &mut BTreeMap<String, usize>,
) -> PathBuf {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("tu");
    let key = if cfg!(windows) {
        stem.to_ascii_lowercase()
    } else {
        stem.to_string()
    };
    let count = taken.entry(key).or_insert(0);
    *count += 1;
    let file = if *count == 1 {
        format!("{stem}.{ext}")
    } else {
        let src_ext = src
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("cpp")
            .to_ascii_lowercase();
        format!("{stem}_{src_ext}.{ext}")
    };
    objs_dir.join(file)
}

/// Compile every source, at most `jobs` (`0`/`None` = cores) at once.
/// All threads join before returning; the first failure wins.
fn compile_all(
    tc: &CxxToolchain,
    sources: &[PathBuf],
    objs_dir: &Path,
    opts: &CompileOptions,
    jobs: Option<usize>,
) -> Result<Vec<PathBuf>, Error> {
    let parallel = jobs
        .filter(|&j| j > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        })
        .clamp(1, sources.len().max(1));
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    let chunk = sources.len().div_ceil(parallel);
    let ext = tc.obj_extension().to_string();
    let mut taken: BTreeMap<String, usize> = BTreeMap::new();
    let mut planned: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(sources.len());
    for src in sources {
        let obj = object_for(objs_dir, src, &ext, &mut taken);
        planned.push((src.clone(), obj));
    }
    let mut ordered: Vec<PathBuf> = Vec::with_capacity(sources.len());
    let mut first_err: Option<Error> = None;
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for group in planned.chunks(chunk) {
            let group = group.to_vec();
            handles.push(s.spawn(move || -> Result<Vec<PathBuf>, Error> {
                let mut done = Vec::with_capacity(group.len());
                for (src, obj) in &group {
                    let mut cmd = tc.compile_command(src, obj, None, opts);
                    run_capture(
                        &mut cmd,
                        &format!("compile {}", src.display()),
                        "read the compiler errors above — they come from the foreign project's own sources",
                        &[],
                    )?;
                    done.push(obj.clone());
                }
                Ok(done)
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(Ok(mut objs)) => ordered.append(&mut objs),
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(_) => {
                    if first_err.is_none() {
                        first_err = Some(Error::InvalidSpec {
                            message: "sources adapter worker thread panicked (internal error)"
                                .to_string(),
                        });
                    }
                }
            }
        }
    });
    if let Some(e) = first_err {
        return Err(e);
    }
    ordered.sort();
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn spec(targets: &[&str], options: &[(&str, &str)]) -> AdapterSpec {
        AdapterSpec {
            tool: "sources".to_string(),
            targets: targets.iter().map(|s| s.to_string()).collect(),
            options: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn ctx_without_toolchain() -> AdapterCtx {
        AdapterCtx::new(PathBuf::from("build"))
    }

    fn write_tree(dir: &Path, files: &[&str]) {
        for f in files {
            let path = dir.join(f);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"x").unwrap();
        }
    }

    #[test]
    fn discovers_top_level_sources_only() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(
            dir.path(),
            &[
                "a.cpp",
                "b.CC",
                "c.cxx",
                "d.c",
                "e.h",
                "sub/f.cpp",
                "note.txt",
            ],
        );
        let found = source_files(dir.path()).unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.cpp", "b.CC", "c.cxx", "d.c"]);
    }

    #[test]
    fn layout_detection_order() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), &["lone.cpp"]);
        // Bare sources beat nothing, lose to every build file.
        assert_eq!(
            detect_layout(dir.path()).unwrap(),
            ForeignLayout::BareSources
        );
        write_tree(dir.path(), &["Makefile"]);
        assert_eq!(detect_layout(dir.path()).unwrap(), ForeignLayout::Make);
        write_tree(dir.path(), &["meson.build"]);
        assert_eq!(detect_layout(dir.path()).unwrap(), ForeignLayout::Meson);
        write_tree(dir.path(), &["CMakeLists.txt"]);
        assert_eq!(detect_layout(dir.path()).unwrap(), ForeignLayout::Cmake);
        write_tree(dir.path(), &["rivet.json"]);
        assert_eq!(detect_layout(dir.path()).unwrap(), ForeignLayout::Manifest);
        // And an empty tree (sources removed) is BareEmpty.
        let empty = tempfile::tempdir().unwrap();
        write_tree(empty.path(), &["README.md"]);
        assert_eq!(
            detect_layout(empty.path()).unwrap(),
            ForeignLayout::BareEmpty
        );
    }

    #[test]
    fn adapter_names_per_layout() {
        assert_eq!(adapter_for_layout(ForeignLayout::Manifest), None);
        assert_eq!(adapter_for_layout(ForeignLayout::Cmake), Some("cmake"));
        assert_eq!(adapter_for_layout(ForeignLayout::Meson), Some("meson"));
        assert_eq!(adapter_for_layout(ForeignLayout::Make), Some("make"));
        assert_eq!(
            adapter_for_layout(ForeignLayout::BareSources),
            Some("sources")
        );
        assert_eq!(adapter_for_layout(ForeignLayout::BareEmpty), None);
    }

    #[test]
    fn spec_validation_rejects_targets_and_options() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_without_toolchain();
        let err = build(dir.path(), &spec(&["a", "b"], &[]), &ctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("at most one"), "got: {err}");
        let err = build(dir.path(), &spec(&[], &[("k", "v")]), &ctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no `options`"), "got: {err}");
    }

    #[test]
    fn missing_toolchain_is_actionable() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), &["a.cpp"]);
        let ctx = ctx_without_toolchain();
        let err = build(dir.path(), &spec(&[], &[]), &ctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("toolchain"), "got: {err}");
    }

    #[test]
    fn empty_tree_discovers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write_tree(dir.path(), &["README.md"]);
        // Reaching `NoLibraries` needs a toolchain (checked before
        // discovery); the discovery half is asserted here, the error
        // itself in CLI end-to-end tests.
        assert!(source_files(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn object_names_disambiguate_collisions() {
        let objs = PathBuf::from("objs");
        let mut taken = BTreeMap::new();
        let a = object_for(&objs, Path::new("a.cpp"), "o", &mut taken);
        let b = object_for(&objs, Path::new("b.cpp"), "o", &mut taken);
        let a2 = object_for(&objs, Path::new("a.cc"), "o", &mut taken);
        assert_eq!(a, objs.join("a.o"));
        assert_eq!(b, objs.join("b.o"));
        assert_eq!(a2, objs.join("a_cc.o"));
    }
}
