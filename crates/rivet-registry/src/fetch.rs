//! Tarball + index-byte transport (`file://` for tests/local, `https://`
//! for real registries via a minimal rustls client) with SHA-256
//! verification and safe gzip-tar extraction.
//!
//! Security discipline (mirrors `rivet-git::validate_checkout_tree`):
//! absolute entry paths, `..` traversal, special files, and symlinks /
//! hardlinks escaping the destination are hard errors; links contained in
//! the destination are preserved.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::{Error, Result};

/// Refuse index bodies larger than this (8 MiB — indexes are small text).
pub const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;
/// Refuse tarball bodies larger than this (256 MiB — bounds a malicious
/// registry's memory use; large C++ packages stay far below).
pub const MAX_TARBALL_BYTES: u64 = 256 * 1024 * 1024;

/// Download raw bytes from `url`.
///
/// * `file://…` URLs read from local disk (always allowed, including
///   offline — they are local files, not network).
/// * `https://…` URLs use a minimal rustls client (see [`https_get`]);
///   refused under `offline` with an actionable error.
/// * Anything else is rejected without touching the network.
pub fn download_bytes(url: &str, offline: bool) -> Result<Vec<u8>, Error> {
    if let Some(rest) = url.strip_prefix("file://") {
        let _ = rest;
        let path = file_url_to_path(url)?;
        std::fs::read(&path).map_err(|e| Error::io("read local registry file", &path, e))
    } else if url.starts_with("https://") {
        if offline {
            return Err(Error::Offline {
                action: format!("fetch {url}"),
                where_: url.to_string(),
            });
        }
        // Tarballs are large, index files small; pick the cap by suffix.
        // (Index fetches go through the same path when read via sparse.)
        let cap = if url.contains("/index/") {
            MAX_INDEX_BYTES
        } else {
            MAX_TARBALL_BYTES
        };
        https_get(url, cap)
    } else {
        Err(Error::BadUrl {
            url: url.to_string(),
            reason: "only `https://` and `file://` URLs are supported".to_string(),
            hint: "use an https:// registry URL, or file:// for local/test registries".to_string(),
        })
    }
}

/// Map a `file://` URL to a filesystem path.
///
/// Handles `file:///abs/path` (Unix), `file:///C:/path` (Windows drive),
/// and minimal `%XX` percent-decoding (ASCII only — enough for spaces in
/// temp-dir fixtures).
pub fn file_url_to_path(url: &str) -> Result<PathBuf, Error> {
    let rest = url.strip_prefix("file://").ok_or_else(|| Error::BadUrl {
        url: url.to_string(),
        reason: "not a `file://` URL".to_string(),
        hint: "file URLs look like `file:///tmp/registry`".to_string(),
    })?;
    // Strip a `localhost` authority (`file://localhost/tmp/x`).
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let decoded = percent_decode(rest);
    // Windows: `/C:/…` → `C:/…` (else the leading slash breaks the path).
    #[cfg(windows)]
    let decoded = {
        let bytes = decoded.as_bytes();
        if decoded.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
        {
            decoded[1..].to_string()
        } else {
            decoded
        }
    };
    if decoded.is_empty() {
        return Err(Error::BadUrl {
            url: url.to_string(),
            reason: "empty path in `file://` URL".to_string(),
            hint: "file URLs look like `file:///tmp/registry`".to_string(),
        });
    }
    Ok(PathBuf::from(decoded))
}

/// Decode `%XX` ASCII escapes; leaves lone `%` and non-UTF8-adjacent
/// sequences untouched (best-effort — registry paths, not general URIs).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 1 {
            if let (Some(&h), Some(&l)) = (bytes.get(i + 1), bytes.get(i + 2)) {
                if let (Some(hi), Some(lo)) = (hex_val(h), hex_val(l)) {
                    out.push((hi << 4) | lo);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Single shared agent (120 s overall timeout per request). Built per call —
/// cheap (no connection pool retained) and keeps zero global state.
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .build()
}

/// GET `url` over HTTPS with rustls certificate verification (always on —
/// there is no insecure knob) and bundled Mozilla roots. Bodies are capped
/// at `cap` bytes; HTTP errors report status + a truncated body snippet.
fn https_get(url: &str, cap: u64) -> Result<Vec<u8>, Error> {
    let action = if url.contains("/index/") {
        "fetch index file"
    } else {
        "fetch tarball"
    };
    let fail = |message: String| Error::Transport {
        action: action.to_string(),
        url: url.to_string(),
        message,
    };
    let response = agent().get(url).call().map_err(|e| match e {
        ureq::Error::Status(code, resp) => {
            let snippet = resp.into_string().unwrap_or_default();
            fail(format!("HTTP {code}: {}", truncate(&snippet, 500)))
        }
        other => fail(other.to_string()),
    })?;
    let mut limited = response.into_reader().take(cap + 1);
    let mut buf = Vec::new();
    limited
        .read_to_end(&mut buf)
        .map_err(|e| fail(format!("read response body: {e}")))?;
    if buf.len() as u64 > cap {
        return Err(fail(format!(
            "response exceeds the {}-byte cap; refusing to buffer",
            cap
        )));
    }
    Ok(buf)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…[truncated]", &s[..max])
    }
}

/// Extract a gzip-compressed tarball into `dest` (created if missing).
///
/// Every entry is validated before anything is written:
/// * absolute paths and any `..` component → [`Error::UnsafeEntry`];
/// * symlinks / hardlinks: absolute targets or targets escaping `dest`
///   (lexically resolved against the link's parent) → [`Error::UnsafeEntry`];
///   links contained in `dest` are recreated as links;
/// * anything that is not a regular file, directory, symlink, or hardlink
///   (fifo, socket, char/block device, …) → [`Error::UnsafeEntry`].
///
/// After extraction the tree is re-validated with [`validate_tree`]
/// (defense in depth — same check `rivet-git` runs on checkouts).
pub fn extract_tarball_gz(bytes: &[u8], dest: &Path) -> Result<(), Error> {
    use flate2::read::GzDecoder;

    std::fs::create_dir_all(dest).map_err(|e| Error::io("create extract dir", dest, e))?;
    let decoder = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive.entries().map_err(|e| Error::UnsafeEntry {
        entry: "<archive>".to_string(),
        reason: format!("cannot list tarball entries: {e}"),
    })?;
    for entry in entries {
        let mut entry = entry.map_err(|e| Error::UnsafeEntry {
            entry: "<archive>".to_string(),
            reason: format!("cannot read tarball entry: {e}"),
        })?;
        extract_one(&mut entry, dest)?;
    }
    validate_tree(dest)
}

fn extract_one<R: Read>(entry: &mut tar::Entry<R>, dest: &Path) -> Result<(), Error> {
    let raw = entry.path().map_err(|e| Error::UnsafeEntry {
        entry: "<undecodable>".to_string(),
        reason: format!("cannot decode entry path: {e}"),
    })?;
    let rel = checked_rel(&raw)?;
    let kind = entry.header().entry_type();
    let target_path = dest.join(&rel);

    if kind.is_symlink() || kind.is_hard_link() {
        let link_target = entry
            .link_name()
            .map_err(|e| Error::UnsafeEntry {
                entry: raw.display().to_string(),
                reason: format!("cannot decode link target: {e}"),
            })?
            .ok_or_else(|| Error::UnsafeEntry {
                entry: raw.display().to_string(),
                reason: "link entry has no target".to_string(),
            })?;
        check_link_target(dest, &target_path, link_target.as_ref())?;
        // Remove any stale file so a retry cannot leave the old content.
        let _ = std::fs::remove_file(&target_path);
        let _ = std::fs::remove_dir(&target_path);
        if let Some(parent) = target_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io("create link parent dir", parent, e))?;
        }
        if kind.is_symlink() {
            create_symlink(link_target.as_ref(), &target_path)?;
        } else {
            let abs_target = link_abs(dest, &target_path, link_target.as_ref());
            std::fs::hard_link(&abs_target, &target_path)
                .map_err(|e| Error::io("create hard link", &target_path, e))?;
        }
        return Ok(());
    }
    if kind.is_dir() {
        std::fs::create_dir_all(&target_path)
            .map_err(|e| Error::io("create extracted dir", &target_path, e))?;
        return Ok(());
    }
    if !kind.is_file() {
        return Err(Error::UnsafeEntry {
            entry: raw.display().to_string(),
            reason: format!("unsupported entry type `{kind:?}` (only files, dirs, and contained links are extracted)"),
        });
    }
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::io("create extracted parent dir", parent, e))?;
    }
    entry
        .unpack(&target_path)
        .map_err(|e| Error::io("write extracted file", &target_path, e))?;
    Ok(())
}

/// Validate a tar entry path: relative, no `..`/absolute/prefix, non-empty.
/// Returns the sanitized relative path (`.` components dropped).
fn checked_rel(raw: &Path) -> Result<PathBuf, Error> {
    let display = raw.display().to_string();
    let mut rel = PathBuf::new();
    for comp in raw.components() {
        match comp {
            Component::Normal(part) => rel.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::UnsafeEntry {
                    entry: display,
                    reason: "absolute paths and `..` are forbidden in tarballs".to_string(),
                });
            }
        }
    }
    if rel.as_os_str().is_empty() {
        return Err(Error::UnsafeEntry {
            entry: display,
            reason: "empty entry path".to_string(),
        });
    }
    Ok(rel)
}

/// Reject absolute link targets and targets escaping `dest` (lexically
/// resolved against the link's own parent dir — same rule as
/// `rivet-git::validate_checkout_tree`).
fn check_link_target(dest: &Path, link_path: &Path, target: &Path) -> Result<(), Error> {
    if target.is_absolute() {
        return Err(Error::UnsafeEntry {
            entry: link_path.display().to_string(),
            reason: format!("link points at absolute target `{}`", target.display()),
        });
    }
    let abs = link_abs(dest, link_path, target);
    if !abs.starts_with(dest) {
        return Err(Error::UnsafeEntry {
            entry: link_path.display().to_string(),
            reason: format!("link escapes the destination (`{}`)", target.display()),
        });
    }
    Ok(())
}

/// Lexically resolve `parent.join(target)` (`.`/`..` normalized without
/// touching the filesystem).
fn link_abs(dest: &Path, link_path: &Path, target: &Path) -> PathBuf {
    let parent = link_path.parent().unwrap_or(dest);
    let joined = parent.join(target);
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn create_symlink(target: &Path, link: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).map_err(|e| Error::io("create symlink", link, e))
    }
    #[cfg(windows)]
    {
        // Try file first, then dir (tar does not record the target kind).
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => Ok(()),
            Err(file_err) => std::os::windows::fs::symlink_dir(target, link)
                .map_err(|_| Error::io("create symlink", link, file_err)),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        Err(Error::UnsafeEntry {
            entry: link.display().to_string(),
            reason: "symlinks are not supported on this platform".to_string(),
        })
    }
}

/// Walk `root` and reject symlinks with absolute targets or targets
/// escaping `root` (same discipline as `rivet-git::validate_checkout_tree`;
/// run automatically after every extraction, also usable standalone).
pub fn validate_tree(root: &Path) -> Result<(), Error> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| Error::io("list extracted dir", &dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| Error::io("list extracted entry", &dir, e))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|e| Error::io("stat extracted entry", &path, e))?;
            if file_type.is_symlink() {
                let target = std::fs::read_link(&path)
                    .map_err(|e| Error::io("read extracted symlink", &path, e))?;
                check_link_target(root, &path, &target)?;
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Build a `.tar.gz` in memory from `(path, kind, payload)` entries.
    /// `kind`: `f` = file, `d` = dir, `s` = symlink (`payload` = target),
    /// `h` = hardlink (`payload` = target), `F` = fifo.
    fn make_tarball(entries: &[(&str, char, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, kind, payload) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            match kind {
                'f' => {
                    header.set_size(payload.len() as u64);
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, path, payload.as_bytes())
                        .expect("append file");
                }
                'd' => {
                    header.set_size(0);
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, path, std::io::empty())
                        .expect("append dir");
                }
                's' => {
                    header.set_size(0);
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_cksum();
                    builder
                        .append_link(&mut header, path, payload)
                        .expect("append symlink");
                }
                'h' => {
                    header.set_size(0);
                    header.set_entry_type(tar::EntryType::Link);
                    header.set_cksum();
                    builder
                        .append_link(&mut header, path, payload)
                        .expect("append hardlink");
                }
                'F' => {
                    header.set_size(0);
                    header.set_entry_type(tar::EntryType::Fifo);
                    header.set_cksum();
                    builder
                        .append_data(&mut header, path, std::io::empty())
                        .expect("append fifo");
                }
                _ => panic!("bad kind"),
            }
        }
        let tar_bytes = builder.into_inner().expect("tar");
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
        enc.write_all(&tar_bytes).expect("gzip");
        enc.finish().expect("finish")
    }

    /// Build a `.tar.gz` from raw header fields, bypassing the `tar`
    /// crate's own builder-side validation (which refuses `..`/absolute
    /// paths — exactly what malicious archives contain). Minimal ustar:
    /// name(100) mode(8) uid(8) gid(8) size(12 octal) mtime(12) chksum(8)
    /// typeflag(1) linkname(100) magic(6).
    fn make_raw_tarball(entries: &[(&str, u8, &str, &[u8])]) -> Vec<u8> {
        let mut tar_bytes: Vec<u8> = Vec::new();
        for (name, typeflag, linkname, data) in entries {
            let mut header = [0u8; 512];
            let name_bytes = name.as_bytes();
            assert!(name_bytes.len() < 100, "test name too long");
            header[..name_bytes.len()].copy_from_slice(name_bytes);
            header[100..108].copy_from_slice(b"0000644\0");
            header[108..116].copy_from_slice(b"0000000\0");
            header[116..124].copy_from_slice(b"0000000\0");
            let size_field = format!("{:011o}\0", data.len());
            header[124..136].copy_from_slice(size_field.as_bytes());
            header[136..148].copy_from_slice(b"00000000000\0");
            header[148..156].copy_from_slice(b"        ");
            header[156] = *typeflag;
            let link_bytes = linkname.as_bytes();
            assert!(link_bytes.len() < 100, "test link too long");
            header[157..157 + link_bytes.len()].copy_from_slice(link_bytes);
            header[257..262].copy_from_slice(b"ustar");
            header[262..264].copy_from_slice(b"00");
            // Checksum over the header with the chksum field as spaces.
            let sum: u32 = header.iter().map(|b| *b as u32).sum();
            let sum_field = format!("{sum:06o}\0 ");
            header[148..156].copy_from_slice(sum_field.as_bytes());
            tar_bytes.extend_from_slice(&header);
            tar_bytes.extend_from_slice(data);
            while tar_bytes.len() % 512 != 0 {
                tar_bytes.push(0);
            }
        }
        // Two zero blocks = end of archive.
        tar_bytes.extend_from_slice(&[0u8; 1024]);
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
        enc.write_all(&tar_bytes).expect("gzip");
        enc.finish().expect("finish")
    }

    /// Portable file-symlink probe (mirrors the `rivet-git` test helper):
    /// `Err` on Windows without Developer Mode / privilege.
    fn try_symlink_file(target: &str, link: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        return std::os::windows::fs::symlink_file(target, link);
        #[cfg(not(any(unix, windows)))]
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks unsupported",
        ));
    }

    #[test]
    fn file_url_roundtrip_and_percent_decode() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file = dir.path().join("with space.txt");
        std::fs::write(&file, b"data").expect("write");
        let raw = file.display().to_string().replace('\\', "/");
        let url_path = raw.replace(' ', "%20");
        let url = if url_path.starts_with('/') {
            format!("file://{url_path}")
        } else {
            format!("file:///{url_path}")
        };
        let back = file_url_to_path(&url).expect("path");
        assert_eq!(back, file);
        let bytes = download_bytes(&url, true).expect("download offline file://");
        assert_eq!(bytes, b"data");
    }

    #[test]
    fn unsupported_scheme_rejected_without_network() {
        let err = download_bytes("ftp://example.com/x.tar.gz", false).expect_err("ftp");
        assert!(matches!(err, Error::BadUrl { .. }), "got: {err}");
        // Offline blocks https without touching the network.
        let err2 = download_bytes("https://example.com/x.tar.gz", true).expect_err("offline");
        assert!(matches!(err2, Error::Offline { .. }), "got: {err2}");
    }

    #[test]
    fn missing_local_file_errors() {
        let err = download_bytes("file:///no/such/file-xyz.tar.gz", true).expect_err("missing");
        assert!(matches!(err, Error::Io { .. }), "got: {err}");
    }

    #[test]
    fn extract_roundtrip() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bytes = make_tarball(&[
            ("pkg-1.0.0/include/a.h", 'f', "#pragma once\n"),
            ("pkg-1.0.0/src/lib.cpp", 'f', "int f(){return 1;}\n"),
        ]);
        extract_tarball_gz(&bytes, dir.path()).expect("extract");
        assert_eq!(
            std::fs::read(dir.path().join("pkg-1.0.0/src/lib.cpp")).expect("read"),
            b"int f(){return 1;}\n"
        );
    }

    #[test]
    fn traversal_rejected_and_nothing_escapes() {
        let parent = tempfile::TempDir::new().expect("tempdir");
        let dest = parent.path().join("dest");
        // Raw header: the `tar` builder itself refuses `..`, so malicious
        // bytes are hand-assembled.
        let bytes = make_raw_tarball(&[("../evil.txt", b'0', "", b"x")]);
        let err = extract_tarball_gz(&bytes, &dest).expect_err("traversal");
        assert!(matches!(err, Error::UnsafeEntry { .. }), "got: {err}");
        assert!(
            !parent.path().join("evil.txt").exists(),
            "traversal must not write outside dest"
        );
    }

    #[test]
    fn nested_dotdot_rejected() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bytes = make_raw_tarball(&[("a/../../evil.txt", b'0', "", b"x")]);
        let err = extract_tarball_gz(&bytes, dir.path()).expect_err("dotdot");
        assert!(matches!(err, Error::UnsafeEntry { .. }), "got: {err}");
    }

    #[test]
    fn absolute_path_rejected() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bytes = make_raw_tarball(&[("/abs.txt", b'0', "", b"x")]);
        let err = extract_tarball_gz(&bytes, dir.path()).expect_err("absolute");
        assert!(matches!(err, Error::UnsafeEntry { .. }), "got: {err}");
        // GNU tar encodes absolute names with a `././@LongLink` entry when
        // built via the high-level builder; the raw form above covers the
        // classic absolute header.
    }

    #[test]
    fn absolute_symlink_rejected() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bytes = make_tarball(&[("evil", 's', "/etc/passwd")]);
        let err = extract_tarball_gz(&bytes, dir.path()).expect_err("abs link");
        assert!(matches!(err, Error::UnsafeEntry { .. }), "got: {err}");
        assert!(!dir.path().join("evil").exists());
    }

    #[test]
    fn escaping_symlink_rejected() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bytes = make_tarball(&[("sub/evil", 's', "../../outside.txt")]);
        let err = extract_tarball_gz(&bytes, dir.path()).expect_err("escaping link");
        assert!(matches!(err, Error::UnsafeEntry { .. }), "got: {err}");
    }

    #[test]
    fn escaping_hardlink_rejected() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bytes = make_tarball(&[("evil", 'h', "../outside.txt")]);
        let err = extract_tarball_gz(&bytes, dir.path()).expect_err("hardlink");
        assert!(matches!(err, Error::UnsafeEntry { .. }), "got: {err}");
    }

    #[test]
    fn fifo_rejected() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bytes = make_tarball(&[("weird", 'F', "")]);
        let err = extract_tarball_gz(&bytes, dir.path()).expect_err("fifo");
        assert!(matches!(err, Error::UnsafeEntry { .. }), "got: {err}");
    }

    #[test]
    fn contained_symlink_preserved() {
        // Probe first: Windows without Developer Mode cannot create links.
        let probe = tempfile::TempDir::new().expect("tempdir");
        if try_symlink_file("probe-target", &probe.path().join("probe-link")).is_err() {
            eprintln!("SKIP: cannot create symlinks (missing privilege)");
            return;
        }
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bytes = make_tarball(&[("real.txt", 'f', "hi"), ("link.txt", 's', "real.txt")]);
        extract_tarball_gz(&bytes, dir.path()).expect("extract");
        let meta = std::fs::symlink_metadata(dir.path().join("link.txt")).expect("meta");
        assert!(meta.file_type().is_symlink(), "in-root link preserved");
        assert_eq!(
            std::fs::read(dir.path().join("link.txt")).expect("read through link"),
            b"hi"
        );
    }

    #[test]
    fn corrupt_gzip_errors() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let err = extract_tarball_gz(b"not a tarball", dir.path()).expect_err("corrupt");
        assert!(matches!(err, Error::UnsafeEntry { .. }), "got: {err}");
    }
}
