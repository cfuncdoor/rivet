//! Incrementality: source-dir hashing + adapter fingerprints.
//!
//! [`fingerprint`] folds (source dir contents + adapter spec + toolchain
//! fingerprint) into one opaque hex string. The integrator stores it next
//! to the built outputs and calls [`needs_rebuild`] before reusing them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rivet_manifest::AdapterSpec;

use super::Error;

/// FNV-1a 64 offset basis.
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
/// FNV-1a 64 prime.
const FNV_PRIME: u64 = 0x100000001b3;

/// Feed `bytes` into the FNV-1a 64 accumulator.
fn feed(mut hash: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Feed a length-prefixed byte string (framing keeps `("ab","c")` and
/// `("a","bc")` distinct).
fn feed_field(hash: u64, bytes: &[u8]) -> u64 {
    let len = (bytes.len() as u64).to_le_bytes();
    feed(feed(hash, &len), bytes)
}

/// Hash a source tree: sorted relative paths (forward slashes) plus file
/// contents. The `.git` subtree is skipped (clone metadata is not build
/// input); directory symlinks are not followed (only the link target
/// string is hashed) so hashing can neither escape the tree nor loop.
pub fn source_dir_hash(dir: &Path) -> Result<u64, Error> {
    let mut rel_paths = Vec::new();
    collect_files(dir, dir, &mut rel_paths)?;
    rel_paths.sort();
    let mut hash = FNV_OFFSET;
    for rel in rel_paths {
        hash = feed_field(hash, rel.as_bytes());
        let full = dir.join(Path::new(&rel));
        // `collect_files` only records files and links; anything that
        // vanished mid-hash is a real IO error.
        let meta = std::fs::symlink_metadata(&full)?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&full)?;
            hash = feed_field(hash, b"link:");
            hash = feed_field(hash, target.to_string_lossy().replace('\\', "/").as_bytes());
        } else {
            hash = feed_field(hash, b"file:");
            hash = feed_field(hash, &std::fs::read(&full)?);
        }
    }
    Ok(hash)
}

/// Collect relative file paths under `dir` (`base` anchors the relative
/// names). Skips `.git` entries and never follows directory symlinks.
fn collect_files(base: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), Error> {
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let ft = entry.file_type()?;
        let name = entry.file_name();
        if name.to_string_lossy() == ".git" {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            collect_files(base, &path, out)?;
        } else if ft.is_file() || ft.is_symlink() {
            let rel = path.strip_prefix(base).map_err(|e| Error::InvalidSpec {
                message: format!("cannot relativize `{}`: {e}", path.display()),
            })?;
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// Canonical adapter-spec string: tool, in-order targets, sorted options.
fn spec_string(spec: &AdapterSpec, options: &BTreeMap<String, String>) -> String {
    let opts: Vec<String> = options.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!(
        "tool={}|targets={}|options={}",
        spec.tool,
        spec.targets.join(","),
        opts.join(",")
    )
}

/// Fingerprint `(source_dir, spec, toolchain_fingerprint)` as an opaque
/// 16-hex-digit string.
///
/// Any change — source bytes added/removed/modified, target list or
/// option edited, toolchain fingerprint changed — flips the value, so the
/// integrator can treat inequality as "rebuild". The format is stable
/// within a rivet release but not a cross-version contract.
pub fn fingerprint(
    source_dir: &Path,
    spec: &AdapterSpec,
    toolchain_fingerprint: &str,
) -> Result<String, Error> {
    if !source_dir.is_dir() {
        return Err(Error::InvalidSpec {
            message: format!(
                "cannot fingerprint `{}`: not a directory. Fix: check the dependency `path`/checkout subdir",
                source_dir.display()
            ),
        });
    }
    let mut hash = FNV_OFFSET;
    // Bind the absolute location too: two different checkouts with
    // identical bytes still rebuild independently (their outputs live in
    // different scratch dirs).
    let anchor: PathBuf =
        std::fs::canonicalize(source_dir).unwrap_or_else(|_| source_dir.to_path_buf());
    hash = feed_field(hash, anchor.to_string_lossy().replace('\\', "/").as_bytes());
    let src_hash = source_dir_hash(source_dir)?.to_le_bytes();
    hash = feed(hash, &src_hash);
    hash = feed_field(hash, spec_string(spec, &spec.options).as_bytes());
    hash = feed_field(hash, toolchain_fingerprint.as_bytes());
    Ok(format!("{hash:016x}"))
}

/// `true` when `previous` (a stored [`fingerprint`]) no longer matches the
/// current inputs — i.e. the adapter-built dep must be rebuilt.
pub fn needs_rebuild(
    previous: &str,
    source_dir: &Path,
    spec: &AdapterSpec,
    toolchain_fingerprint: &str,
) -> Result<bool, Error> {
    Ok(fingerprint(source_dir, spec, toolchain_fingerprint)? != previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with(targets: &[&str], options: &[(&str, &str)]) -> AdapterSpec {
        AdapterSpec {
            tool: "cmake".to_string(),
            targets: targets.iter().map(|s| s.to_string()).collect(),
            options: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("include")).unwrap();
        std::fs::write(dir.path().join("CMakeLists.txt"), b"project(x)").unwrap();
        std::fs::write(dir.path().join("a.cpp"), b"int f();").unwrap();
        dir
    }

    #[test]
    fn fingerprint_stable_and_spec_sensitive() {
        let dir = fixture();
        let base = spec_with(&["mini"], &[("A", "1")]);
        let fp1 = fingerprint(dir.path(), &base, "tc1").unwrap();
        let fp2 = fingerprint(dir.path(), &base, "tc1").unwrap();
        assert_eq!(fp1, fp2);
        assert_eq!(fp1.len(), 16);
        // Targets change.
        let other = spec_with(&["mini", "extra"], &[("A", "1")]);
        assert_ne!(fp1, fingerprint(dir.path(), &other, "tc1").unwrap());
        // Options change.
        let other = spec_with(&["mini"], &[("A", "2")]);
        assert_ne!(fp1, fingerprint(dir.path(), &other, "tc1").unwrap());
        // Tool change.
        let mut other = base.clone();
        other.tool = "meson".to_string();
        assert_ne!(fp1, fingerprint(dir.path(), &other, "tc1").unwrap());
        // Toolchain change.
        assert_ne!(fp1, fingerprint(dir.path(), &base, "tc2").unwrap());
    }

    #[test]
    fn source_edits_change_fingerprint() {
        let dir = fixture();
        let spec = spec_with(&[], &[]);
        let before = fingerprint(dir.path(), &spec, "tc").unwrap();
        std::fs::write(dir.path().join("a.cpp"), b"int f() { return 1; }").unwrap();
        assert_ne!(before, fingerprint(dir.path(), &spec, "tc").unwrap());
        // New file too.
        std::fs::write(dir.path().join("b.cpp"), b"x").unwrap();
        let after_add = fingerprint(dir.path(), &spec, "tc").unwrap();
        // ...but `.git` metadata is not build input.
        std::fs::create_dir_all(dir.path().join(".git").join("objects")).unwrap();
        std::fs::write(dir.path().join(".git").join("HEAD"), b"ref: x").unwrap();
        std::fs::write(
            dir.path().join(".git").join("objects").join("pack"),
            b"junk",
        )
        .unwrap();
        assert_eq!(after_add, fingerprint(dir.path(), &spec, "tc").unwrap());
    }

    #[test]
    fn needs_rebuild_roundtrip() {
        let dir = fixture();
        let spec = spec_with(&["mini"], &[]);
        let fp = fingerprint(dir.path(), &spec, "tc").unwrap();
        assert!(!needs_rebuild(&fp, dir.path(), &spec, "tc").unwrap());
        assert!(needs_rebuild("deadbeefdeadbeef", dir.path(), &spec, "tc").unwrap());
        std::fs::write(dir.path().join("a.cpp"), b"changed").unwrap();
        assert!(needs_rebuild(&fp, dir.path(), &spec, "tc").unwrap());
    }

    #[test]
    fn missing_dir_is_actionable() {
        let spec = spec_with(&[], &[]);
        let err = fingerprint(Path::new("no-such-dir"), &spec, "tc")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no-such-dir"), "got: {err}");
    }
}
