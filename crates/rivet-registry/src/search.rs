//! Index search over a local index directory: substring match on
//! name/description, returning the latest non-yanked version per package.
//!
//! Only local index copies are searchable (a sparse registry has no package
//! listing endpoint in M5 — the CLI syncs an index copy first, then
//! searches it offline).

use std::path::Path;

use crate::{Error, RegistryIndex, Result};

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRow {
    /// Package name.
    pub name: String,
    /// Latest non-yanked version.
    pub version: String,
    /// Its description, if the index line carries one.
    pub description: Option<String>,
}

/// Search `index_dir` for `query` (case-insensitive substring over the
/// package name and description; empty query matches everything).
/// Packages whose every version is yanked are skipped. Rows sort by name.
/// A corrupt package file is an error (fail fast — a corrupt index copy
/// must not search silently).
pub fn search(index_dir: &Path, query: &str) -> Result<Vec<SearchRow>, Error> {
    let index = RegistryIndex::local(index_dir.to_path_buf());
    let mut names: Vec<String> = Vec::new();
    let entries =
        std::fs::read_dir(index_dir).map_err(|e| Error::io("list index dir", index_dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io("list index entry", index_dir, e))?;
        let file_type = entry
            .file_type()
            .map_err(|e| Error::io("stat index entry", &entry.path(), e))?;
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !crate::is_valid_package_name(&name) {
            continue;
        }
        names.push(name);
    }
    names.sort();
    names.dedup();
    let needle = query.to_lowercase();
    let mut rows = Vec::new();
    for name in &names {
        let versions = index.versions(name)?;
        let mut avail: Vec<_> = versions.into_iter().filter(|e| !e.yanked).collect();
        if avail.is_empty() {
            continue;
        }
        avail.sort_by(|a, b| {
            semver::Version::parse(&a.version)
                .unwrap_or_else(|_| semver::Version::new(0, 0, 0))
                .cmp(
                    &semver::Version::parse(&b.version)
                        .unwrap_or_else(|_| semver::Version::new(0, 0, 0)),
                )
        });
        let Some(latest) = avail.last() else {
            continue;
        };
        let description = latest.description.clone();
        if !needle.is_empty() {
            let hay_name = name.to_lowercase();
            let hay_desc = description.clone().unwrap_or_default().to_lowercase();
            if !hay_name.contains(&needle) && !hay_desc.contains(&needle) {
                continue;
            }
        }
        rows.push(SearchRow {
            name: name.clone(),
            version: latest.version.clone(),
            description,
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let foo = [
            "{\"version\":\"1.0.0\",\"checksum\":\"sha256:aa\",\"yanked\":false,\"deps\":[],\"description\":\"Fast vectors\"}",
            "{\"version\":\"1.2.0\",\"checksum\":\"sha256:bb\",\"yanked\":false,\"deps\":[],\"description\":\"Fast vectors 2\"}",
            "{\"version\":\"2.0.0\",\"checksum\":\"sha256:cc\",\"yanked\":true,\"deps\":[]}",
        ]
        .join("\n");
        let bar =
            "{\"version\":\"0.3.0\",\"checksum\":\"sha256:dd\",\"yanked\":false,\"deps\":[],\"description\":\"JSON parser\"}";
        // Fully yanked: never surfaces.
        let dead = "{\"version\":\"9.9.9\",\"checksum\":\"sha256:ee\",\"yanked\":true,\"deps\":[]}";
        std::fs::write(dir.path().join("foo"), foo).expect("write");
        std::fs::write(dir.path().join("bar"), bar).expect("write");
        std::fs::write(dir.path().join("dead"), dead).expect("write");
        dir
    }

    #[test]
    fn search_matches_and_sorts() {
        let dir = fixture();
        let rows = search(dir.path(), "").expect("search");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "bar");
        assert_eq!(rows[1].name, "foo");
        // Latest non-yanked (2.0.0 is yanked → skipped).
        assert_eq!(rows[1].version, "1.2.0");
        assert_eq!(rows[1].description.as_deref(), Some("Fast vectors 2"));
    }

    #[test]
    fn search_filters_by_name_and_description() {
        let dir = fixture();
        let rows = search(dir.path(), "vec").expect("search");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "foo");
        let rows = search(dir.path(), "JSON").expect("search");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "bar");
        assert!(search(dir.path(), "nothing-here")
            .expect("search")
            .is_empty());
    }

    #[test]
    fn search_skips_fully_yanked() {
        let dir = fixture();
        assert!(
            search(dir.path(), "dead").expect("search").is_empty(),
            "fully-yanked packages never surface"
        );
    }
}
