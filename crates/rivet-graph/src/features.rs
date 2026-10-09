//! Feature activation (contract C1 — frozen, shared with the resolver).
//!
//! A package's `features` table maps feature names to entries. An entry is
//! either `dep:<name>` (enables the optional dependency `<name>`) or another
//! feature name: a same-package name enables it transitively; anything else
//! (e.g. cross-package `other/feat` syntax) is left for the resolver and
//! ignored here.
//!
//! Enabled features per package = union of: `default` (unless
//! `--no-default-features`), `--features` requests, everything (if
//! `--all-features`). Unknown requested names and `dep:` entries pointing
//! at nonexistent or non-optional deps are errors. The enabled set (sorted)
//! joins the build fingerprint; optional path deps not enabled are filtered
//! out of the build in [`crate::workspace`].

use std::collections::{BTreeSet, VecDeque};

use rivet_manifest::Manifest;

use super::Error;

/// CLI feature selection, mirroring Cargo's `--features` / `--all-features`
/// / `--no-default-features`.
#[derive(Debug, Clone, Default)]
pub struct FeatureFlags {
    /// Requested features (`--features a,b --features c` accumulates).
    pub features: Vec<String>,
    /// Enable every feature in the table (`--all-features`).
    pub all: bool,
    /// Skip the `default` feature (`--no-default-features`).
    pub no_default: bool,
}

/// Compute the enabled feature set for `manifest` under `flags`.
///
/// Errors on unknown requested names and on `dep:` entries (of enabled
/// features only) that name nonexistent or non-optional dependencies.
///
/// This is the strict per-package helper the resolver reuses with
/// package-targeted request lists (e.g. from the lockfile's per-package
/// `features`). Command-line requests, which name features of the selected
/// packages, go through [`activated_features_lenient`] + a scope-wide
/// known-feature check in [`crate::workspace`] instead: a request unknown
/// to one dependency must not fail the whole build, while a request
/// unknown to *every* package in scope is still a typo error.
pub fn activated_features(
    manifest: &Manifest,
    flags: &FeatureFlags,
) -> Result<BTreeSet<String>, Error> {
    let table = &manifest.features;
    let mut enabled: BTreeSet<String> = BTreeSet::new();
    if flags.all {
        enabled.extend(table.keys().cloned());
    } else {
        if !flags.no_default && table.contains_key("default") {
            enabled.insert("default".to_string());
        }
        for name in &flags.features {
            if !table.contains_key(name) {
                let mut available: Vec<String> = table.keys().cloned().collect();
                available.sort();
                return Err(Error::UnknownFeature {
                    name: name.clone(),
                    available: if available.is_empty() {
                        "<none>".to_string()
                    } else {
                        available.join(", ")
                    },
                });
            }
            enabled.insert(name.clone());
        }
    }
    // Transitive closure over same-package feature references.
    let mut queue: VecDeque<String> = enabled.iter().cloned().collect();
    while let Some(name) = queue.pop_front() {
        let entries = match table.get(&name) {
            Some(e) => e.clone(),
            None => continue,
        };
        for entry in &entries {
            if entry.starts_with("dep:") {
                continue;
            }
            if table.contains_key(entry) && enabled.insert(entry.clone()) {
                queue.push_back(entry.clone());
            }
            // Anything else (e.g. `other-pkg/feat`) belongs to cross-package
            // unification in the resolver; ignored here.
        }
    }
    // Validate `dep:` entries of enabled features (disabled features may
    // carry entries that only matter when enabled).
    for name in &enabled {
        let entries = match table.get(name) {
            Some(e) => e.clone(),
            None => continue,
        };
        for entry in &entries {
            if let Some(dep_name) = entry.strip_prefix("dep:") {
                match manifest.dependencies.get(dep_name) {
                    None => {
                        return Err(Error::UnknownFeatureDep {
                            feature: name.clone(),
                            name: dep_name.to_string(),
                        });
                    }
                    Some(dep) if !dep.is_optional() => {
                        return Err(Error::NonOptionalFeatureDep {
                            feature: name.clone(),
                            name: dep_name.to_string(),
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(enabled)
}

/// Lenient form of [`activated_features`]: requests that name no feature
/// of `manifest` are ignored instead of erroring, so one `--features` list
/// can span packages that define different features (unification = union of
/// requested features per package). Callers must separately reject requests
/// unknown to every package in scope (typo protection); `dep:` validation
/// still errors exactly like the strict form.
pub fn activated_features_lenient(
    manifest: &Manifest,
    flags: &FeatureFlags,
) -> Result<BTreeSet<String>, Error> {
    let mine: Vec<String> = flags
        .features
        .iter()
        .filter(|f| manifest.features.contains_key(*f))
        .cloned()
        .collect();
    activated_features(
        manifest,
        &FeatureFlags {
            features: mine,
            all: flags.all,
            no_default: flags.no_default,
        },
    )
}

/// Optional dependency names activated by `enabled` (the `dep:<name>`
/// entries across all enabled features).
pub fn active_optional_deps(manifest: &Manifest, enabled: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for name in enabled {
        if let Some(entries) = manifest.features.get(name) {
            for entry in entries {
                if let Some(dep_name) = entry.strip_prefix("dep:") {
                    out.insert(dep_name.to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_in(dir: &std::path::Path, json: &str) -> Manifest {
        std::fs::write(dir.join("rivet.json"), json).unwrap();
        let mut m = rivet_manifest::parse_str(json).unwrap();
        m.dir = dir.to_path_buf();
        m
    }

    fn pkg_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn default_feature_enabled_unless_suppressed() {
        let dir = pkg_dir();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"features\":{\"default\":[],\"extra\":[]}}",
        );
        let enabled = activated_features(&m, &FeatureFlags::default()).unwrap();
        assert!(enabled.contains("default"));
        assert!(!enabled.contains("extra"));

        let suppressed = activated_features(
            &m,
            &FeatureFlags {
                no_default: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!suppressed.contains("default"));
    }

    #[test]
    fn requested_and_all_features() {
        let dir = pkg_dir();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"features\":{\"default\":[],\"extra\":[],\"more\":[]}}",
        );
        let enabled = activated_features(
            &m,
            &FeatureFlags {
                features: vec!["extra".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(enabled.contains("default"));
        assert!(enabled.contains("extra"));
        assert!(!enabled.contains("more"));

        let all = activated_features(
            &m,
            &FeatureFlags {
                all: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn unknown_requested_feature_errors() {
        let dir = pkg_dir();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"features\":{\"default\":[]}}",
        );
        let err = activated_features(
            &m,
            &FeatureFlags {
                features: vec!["nope".to_string()],
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown feature"), "got: {err}");
        assert!(err.contains("nope"), "got: {err}");
    }

    #[test]
    fn dep_entries_validate_optionality() {
        let dir = pkg_dir();
        let json = "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"dependencies\":{\"extra\":{\"path\":\"../extra\",\"optional\":true}},\"features\":{\"default\":[],\"extra\":[\"dep:extra\"]}}";
        let m = manifest_in(dir.path(), json);
        let enabled = activated_features(
            &m,
            &FeatureFlags {
                features: vec!["extra".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(enabled.contains("extra"));
        let active = active_optional_deps(&m, &enabled);
        assert!(active.contains("extra"));

        // `dep:` at a missing dep is an error.
        let bad = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"features\":{\"broken\":[\"dep:ghost\"]}}",
        );
        let err = activated_features(
            &bad,
            &FeatureFlags {
                features: vec!["broken".to_string()],
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("ghost"), "got: {err}");

        // `dep:` at a non-optional dep is an error.
        let req = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"dependencies\":{\"req\":{\"path\":\"../req\"}},\"features\":{\"broken\":[\"dep:req\"]}}",
        );
        let err = activated_features(
            &req,
            &FeatureFlags {
                features: vec!["broken".to_string()],
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not optional"), "got: {err}");
    }

    #[test]
    fn local_feature_refs_enable_transitively() {
        let dir = pkg_dir();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"features\":{\"full\":[\"extra\"],\"extra\":[]}}",
        );
        let enabled = activated_features(
            &m,
            &FeatureFlags {
                features: vec!["full".to_string()],
                no_default: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(enabled.contains("full"));
        assert!(enabled.contains("extra"));
    }

    #[test]
    fn lenient_ignores_foreign_requests_but_validates_deps() {
        let dir = pkg_dir();
        // No `with-opt` here (e.g. a dependency's table): ignored, no error.
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"dep\",\"version\":\"0.1.0\"},\"features\":{\"default\":[]}}",
        );
        let enabled = activated_features_lenient(
            &m,
            &FeatureFlags {
                features: vec!["with-opt".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(enabled, BTreeSet::from(["default".to_string()]));

        // `dep:` validation still applies to enabled features.
        let bad = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"dep\",\"version\":\"0.1.0\"},\"features\":{\"broken\":[\"dep:ghost\"]}}",
        );
        let err = activated_features_lenient(
            &bad,
            &FeatureFlags {
                features: vec!["broken".to_string(), "with-opt".to_string()],
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("ghost"), "got: {err}");
    }

    #[test]
    fn cross_package_entries_ignored_for_m4() {
        let dir = pkg_dir();
        let m = manifest_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"features\":{\"f\":[\"other/feat\"]}}",
        );
        let enabled = activated_features(
            &m,
            &FeatureFlags {
                features: vec!["f".to_string()],
                no_default: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(enabled, BTreeSet::from(["f".to_string()]));
    }
}
