//! Deterministic source packaging: a directory becomes a gzip tarball with
//! sorted entries, fixed metadata, and no `target/` or `.git` content.
//!
//! Rules (documented for the CLI integrator and the future server, which
//! must verify the checksum, not the byte layout):
//! * Every archive path is prefixed `<name>-<version>/` (Cargo convention).
//! * Entries are sorted by archive path; file mode `0o644`, dir mode
//!   `0o755`, uid/gid `0`, mtime [`FIXED_MTIME`], empty user/group.
//! * Any path with a `target` or `.git` component is excluded (build output
//!   and VCS metadata must never ship).
//! * Symlinks are rejected (actionable error — replace with a real file).
//!   Our extractor preserves contained links, but packages we produce never
//!   contain them, so publish → materialize always round-trips.
//! * gzip uses fixed compression level 6 with a zeroed header; packaging
//!   the same directory twice yields identical bytes.

use std::io::Write as _;
use std::path::{Component, Path};

use crate::{Error, IndexDep, Result};

/// Fixed tar entry mtime (epoch) for deterministic packages.
pub const FIXED_MTIME: u64 = 0;

/// File mode for packaged files.
const FILE_MODE: u32 = 0o644;
/// Dir mode for packaged dirs.
const DIR_MODE: u32 = 0o755;

/// Metadata describing the package being published.
#[derive(Debug, Clone)]
pub struct PublishMeta {
    /// Package name (`[A-Za-z0-9_-]+`).
    pub name: String,
    /// Package version (`major.minor.patch[-pre][+build]`).
    pub version: String,
    /// One-line description (stored in the index entry by the server).
    pub description: Option<String>,
    /// Transitive deps (stored in the index entry by the server).
    pub deps: Vec<IndexDep>,
}

/// A packaged source dir: deterministic bytes + its `sha256:…` checksum.
#[derive(Debug, Clone)]
pub struct PackagedTarball {
    /// Package name.
    pub name: String,
    /// Package version.
    pub version: String,
    /// Deterministic `.tar.gz` bytes.
    pub bytes: Vec<u8>,
    /// `sha256:…` over [`PackagedTarball::bytes`] (lockfile form).
    pub checksum: String,
}

/// Package `src_dir` into a deterministic tarball per [`PublishMeta`].
///
/// Reads (never writes) `src_dir`; `rivet.json` is packed like any other
/// file — name/version agreement with the manifest is the CLI's job (it
/// owns manifests; this crate stays manifest-free).
pub fn package_source_dir(src_dir: &Path, meta: &PublishMeta) -> Result<PackagedTarball, Error> {
    if !crate::is_valid_package_name(&meta.name) {
        return Err(Error::Package {
            path: src_dir.display().to_string(),
            message: format!("invalid package name `{}`", meta.name),
            hint: "package names use only [a-zA-Z0-9_-]".to_string(),
        });
    }
    semver::Version::parse(&meta.version).map_err(|e| Error::Package {
        path: src_dir.display().to_string(),
        message: format!("invalid version `{}`: {e}", meta.version),
        hint: "versions must be `major.minor.patch`, e.g. `0.1.0`".to_string(),
    })?;
    if !src_dir.is_dir() {
        return Err(Error::Package {
            path: src_dir.display().to_string(),
            message: "not a directory".to_string(),
            hint: "point publish at the package source directory".to_string(),
        });
    }

    // Collect (archive-path, fs-path-or-none-for-dirs, file-bytes).
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    let prefix = format!("{}-{}/", meta.name, meta.version);
    let mut walker = walkdir::WalkDir::new(src_dir)
        .follow_links(false)
        .min_depth(1)
        .into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry.map_err(|e| Error::Package {
            path: src_dir.display().to_string(),
            message: format!("cannot walk source dir: {e}"),
            hint: "check permissions under the source directory".to_string(),
        })?;
        let rel = entry
            .path()
            .strip_prefix(src_dir)
            .map_err(|e| Error::Package {
                path: src_dir.display().to_string(),
                message: format!("cannot relativize `{}`: {e}", entry.path().display()),
                hint: "report a bug".to_string(),
            })?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        if has_excluded_component(rel) {
            if entry.file_type().is_dir() {
                walker.skip_current_dir();
            }
            continue;
        }
        if entry.file_type().is_symlink() {
            return Err(Error::Package {
                path: src_dir.display().to_string(),
                message: format!("symlink `{}` would not round-trip", rel.display()),
                hint: "replace the symlink with a real file or directory".to_string(),
            });
        }
        // Archive paths always use `/` (portable across Windows/Unix).
        let arc = format!("{prefix}{}", rel_to_slash(rel)?);
        if entry.file_type().is_dir() {
            dirs.push(arc);
        } else if entry.file_type().is_file() {
            let bytes = std::fs::read(entry.path())
                .map_err(|e| Error::io("read source file", entry.path(), e))?;
            files.push((arc, bytes));
        }
        // Sockets/fifos/etc. are silently absent from C++ sources in
        // practice; anything else non-file non-dir is skipped so packaging
        // cannot hang on a fifo.
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    dirs.sort();

    let mut tar_buf: Vec<u8> = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        append_dir(&mut builder, &prefix)?;
        for dir in &dirs {
            append_dir(&mut builder, dir)?;
        }
        for (arc, bytes) in &files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(FILE_MODE);
            header.set_mtime(FIXED_MTIME);
            header.set_uid(0);
            header.set_gid(0);
            header.set_cksum();
            builder
                .append_data(&mut header, arc, bytes.as_slice())
                .map_err(|e| Error::Package {
                    path: src_dir.display().to_string(),
                    message: format!("cannot append `{arc}` to the tarball: {e}"),
                    hint: "check for over-long paths (>100 chars need GNU headers)".to_string(),
                })?;
        }
        builder.into_inner().map_err(|e| Error::Package {
            path: src_dir.display().to_string(),
            message: format!("cannot finish the tarball: {e}"),
            hint: "report a bug".to_string(),
        })?;
    }
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
    enc.write_all(&tar_buf).map_err(|e| Error::Package {
        path: src_dir.display().to_string(),
        message: format!("gzip failed: {e}"),
        hint: "report a bug".to_string(),
    })?;
    let bytes = enc.finish().map_err(|e| Error::Package {
        path: src_dir.display().to_string(),
        message: format!("gzip finish failed: {e}"),
        hint: "report a bug".to_string(),
    })?;
    let checksum = crate::sha256_checksum(&bytes);
    Ok(PackagedTarball {
        name: meta.name.clone(),
        version: meta.version.clone(),
        bytes,
        checksum,
    })
}

fn append_dir(builder: &mut tar::Builder<&mut Vec<u8>>, arc: &str) -> Result<(), Error> {
    let mut header = tar::Header::new_gnu();
    header.set_size(0);
    header.set_mode(DIR_MODE);
    header.set_mtime(FIXED_MTIME);
    header.set_uid(0);
    header.set_gid(0);
    header.set_entry_type(tar::EntryType::Directory);
    header.set_cksum();
    builder
        .append_data(&mut header, arc, std::io::empty())
        .map_err(|e| Error::Package {
            path: arc.to_string(),
            message: format!("cannot append dir to the tarball: {e}"),
            hint: "report a bug".to_string(),
        })
}

fn has_excluded_component(rel: &Path) -> bool {
    rel.components().any(|c| match c {
        Component::Normal(part) => part == "target" || part == ".git",
        _ => false,
    })
}

/// Render a walked relative path with `/` separators (portable across
/// Windows/Unix). Only `Normal` components are kept; anything else from a
/// directory walk is a bug.
fn rel_to_slash(rel: &Path) -> Result<String, Error> {
    let mut parts: Vec<&str> = Vec::new();
    for comp in rel.components() {
        match comp {
            Component::Normal(part) => {
                parts.push(part.to_str().ok_or_else(|| Error::Package {
                    path: rel.display().to_string(),
                    message: "non-UTF8 file name".to_string(),
                    hint: "rename the file to UTF-8".to_string(),
                })?);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::Package {
                    path: rel.display().to_string(),
                    message: "unexpected absolute/`..` path while walking".to_string(),
                    hint: "report a bug".to_string(),
                });
            }
        }
    }
    if parts.is_empty() {
        return Err(Error::Package {
            path: rel.display().to_string(),
            message: "empty relative path while walking".to_string(),
            hint: "report a bug".to_string(),
        });
    }
    Ok(parts.join("/"))
}

/// List entry paths of a gzip tarball (debugging/tests; stored order).
pub fn tarball_entries(bytes: &[u8]) -> Result<Vec<String>, Error> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let mut out = Vec::new();
    let entries = archive.entries().map_err(|e| Error::UnsafeEntry {
        entry: "<archive>".to_string(),
        reason: format!("cannot list tarball entries: {e}"),
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::UnsafeEntry {
            entry: "<archive>".to_string(),
            reason: format!("cannot read tarball entry: {e}"),
        })?;
        let path = entry.header().path().map_err(|e| Error::UnsafeEntry {
            entry: "<undecodable>".to_string(),
            reason: format!("cannot decode entry path: {e}"),
        })?;
        out.push(path.display().to_string().replace('\\', "/"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(name: &str, version: &str) -> PublishMeta {
        PublishMeta {
            name: name.to_string(),
            version: version.to_string(),
            description: None,
            deps: Vec::new(),
        }
    }

    fn sample_src() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("include")).expect("mkdir");
        std::fs::write(dir.path().join("rivet.json"), "{\"package\":{}}").expect("write");
        std::fs::write(dir.path().join("src/lib.cpp"), "int f(){return 1;}\n").expect("write");
        std::fs::write(dir.path().join("include/a.h"), "#pragma once\n").expect("write");
        dir
    }

    #[test]
    fn package_twice_is_byte_identical() {
        let src = sample_src();
        let m = meta("foo", "1.0.0");
        let a = package_source_dir(src.path(), &m).expect("package");
        let b = package_source_dir(src.path(), &m).expect("package");
        assert_eq!(a.bytes, b.bytes, "deterministic packaging required");
        assert_eq!(a.checksum, b.checksum);
        assert!(a.checksum.starts_with("sha256:"));
        // Verify with our own parser (shape check doubles as server-side check).
        crate::parse_checksum(&a.checksum).expect("shape");
    }

    #[test]
    fn package_skips_target_and_git() {
        let src = sample_src();
        std::fs::create_dir_all(src.path().join("target/debug")).expect("mkdir");
        std::fs::write(src.path().join("target/debug/x.o"), "junk").expect("write");
        std::fs::create_dir_all(src.path().join(".git/objects")).expect("mkdir");
        std::fs::write(src.path().join(".git/HEAD"), "junk").expect("write");
        std::fs::create_dir_all(src.path().join("src/target")).expect("mkdir");
        std::fs::write(src.path().join("src/target/nested.cpp"), "junk").expect("write");
        let packed = package_source_dir(src.path(), &meta("foo", "1.0.0")).expect("package");
        let entries = tarball_entries(&packed.bytes).expect("entries");
        assert!(
            entries
                .iter()
                .all(|e| !e.contains("target/") && !e.contains(".git")),
            "excluded content leaked: {entries:?}"
        );
        assert!(
            entries.iter().any(|e| e == "foo-1.0.0/src/lib.cpp"),
            "real sources missing: {entries:?}"
        );
    }

    #[test]
    fn package_rejects_symlinks() {
        let src = sample_src();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink("src/lib.cpp", src.path().join("link.cpp")).is_ok();
        #[cfg(windows)]
        let linked =
            std::os::windows::fs::symlink_file("src/lib.cpp", src.path().join("link.cpp")).is_ok();
        #[cfg(not(any(unix, windows)))]
        let linked = false;
        if !linked {
            eprintln!("SKIP: cannot create symlinks (missing privilege)");
            return;
        }
        let err = package_source_dir(src.path(), &meta("foo", "1.0.0")).expect_err("symlink");
        assert!(matches!(err, Error::Package { .. }), "got: {err}");
    }

    #[test]
    fn package_rejects_bad_names() {
        let src = sample_src();
        assert!(package_source_dir(src.path(), &meta("../evil", "1.0.0")).is_err());
        assert!(package_source_dir(src.path(), &meta("foo", "banana")).is_err());
        assert!(package_source_dir(&src.path().join("missing"), &meta("foo", "1.0.0")).is_err());
    }

    #[test]
    fn package_then_extract_roundtrip() {
        let src = sample_src();
        let packed = package_source_dir(src.path(), &meta("foo", "1.0.0")).expect("package");
        let dest = tempfile::TempDir::new().expect("tempdir");
        crate::extract_tarball_gz(&packed.bytes, dest.path()).expect("extract");
        assert_eq!(
            std::fs::read(dest.path().join("foo-1.0.0/src/lib.cpp")).expect("read"),
            b"int f(){return 1;}\n"
        );
    }
}
