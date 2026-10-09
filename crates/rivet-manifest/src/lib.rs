//! `rivet.json` manifest parsing and typed model.
//!
//! The manifest is JSONC: `//` and `/* */` comments plus trailing commas are
//! allowed. [`strip_jsonc`] removes them in a small lexing pass that replaces
//! stripped characters with spaces (newlines preserved), so every byte offset
//! — and therefore every line/column — in the stripped text matches the
//! original source. [`parse_str`] / [`load`] then deserialize with
//! `serde_json`, and all errors carry `line:column` plus a JSON path such as
//! `dependencies.fmt.git`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A manifest error with source location and JSON path.
#[derive(Debug, Error)]
pub enum Error {
    /// Filesystem / IO failure while loading `rivet.json`.
    #[error("{path}: {message}")]
    Io {
        /// File that failed to load.
        path: String,
        /// What happened and how to fix it.
        message: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },
    /// Syntax or validation failure.
    #[error("{path}: {message} at {line}:{column}")]
    Parse {
        /// JSON path, e.g. `dependencies.fmt.git` (`$` for whole-document syntax errors).
        path: String,
        /// What failed and how to fix it.
        message: String,
        /// 1-based line number in the original `rivet.json`.
        line: usize,
        /// 1-based column number in the original `rivet.json`.
        column: usize,
    },
}

impl Error {
    fn parse(
        path: impl Into<String>,
        message: impl Into<String>,
        line: usize,
        column: usize,
    ) -> Self {
        Self::Parse {
            path: path.into(),
            message: message.into(),
            line,
            column,
        }
    }
}

// ---------------------------------------------------------------------------
// JSONC stripping (line/column preserving)
// ---------------------------------------------------------------------------

/// Strip JSONC extensions (line/block comments, trailing commas) from `input`.
///
/// Every stripped character except newlines is replaced with a space, so the
/// returned string has identical line/column mapping to the input. The result
/// is strict JSON suitable for `serde_json`.
pub fn strip_jsonc(input: &str) -> String {
    // Pass 1: strip comments (string-aware).
    let without_comments = strip_comments(input);
    // Pass 2: strip trailing commas (string-aware, comments already gone).
    strip_trailing_commas(&without_comments)
}

fn strip_comments(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;

    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            out.push(c as char);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        // Not in string.
        if c == b'"' {
            in_string = true;
            out.push('"');
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            // Line comment: blank until (not including) newline.
            out.push(' ');
            out.push(' ');
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            // Block comment: blank everything, preserve newlines.
            out.push(' ');
            out.push(' ');
            i += 2;
            while i < bytes.len() {
                if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    break;
                }
                if bytes[i] == b'\n' {
                    out.push('\n');
                } else {
                    out.push(' ');
                }
                i += 1;
            }
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

fn strip_trailing_commas(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    out.extend_from_slice(bytes);
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < out.len() {
        let c = out[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if c == b',' {
            // Look ahead over whitespace; if next non-ws is } or ], blank comma.
            let mut j = i + 1;
            while j < out.len()
                && (out[j] == b' ' || out[j] == b'\t' || out[j] == b'\n' || out[j] == b'\r')
            {
                j += 1;
            }
            if j < out.len() && (out[j] == b'}' || out[j] == b']') {
                out[i] = b' ';
            }
            i += 1;
            continue;
        }
        i += 1;
    }
    // SAFETY: we only replaced ASCII `,` with ASCII space; UTF-8 intact.
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

/// Convert a byte offset into 1-based (line, column).
fn line_col_of(text: &str, offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (idx, ch) in text.char_indices() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Best-effort location of a JSON key path's last segment in `text`.
/// Returns (line, column) of the opening quote of `"key"`, or (1,1).
fn locate_key(text: &str, dotted_path: &str) -> (usize, usize) {
    let key = dotted_path.rsplit('.').next().unwrap_or(dotted_path);
    let needle = format!("\"{key}\"");
    if let Some(off) = text.rfind(&needle) {
        line_col_of(text, off)
    } else {
        (1, 1)
    }
}

// ---------------------------------------------------------------------------
// Typed model
// ---------------------------------------------------------------------------

/// Top-level manifest (`rivet.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// JSON Schema reference (optional, but `rivet new` always emits it).
    #[serde(rename = "$schema", default)]
    pub schema: Option<String>,
    /// Package metadata (required).
    pub package: Package,
    /// Runtime dependencies (optional).
    #[serde(default)]
    pub dependencies: BTreeMap<String, Dependency>,
    /// Dev dependencies (optional, M3+).
    #[serde(default, rename = "devDependencies")]
    pub dev_dependencies: BTreeMap<String, Dependency>,
    /// Build dependencies (optional, M7+).
    #[serde(default, rename = "buildDependencies")]
    pub build_dependencies: BTreeMap<String, Dependency>,
    /// Feature flags (optional, M3+; parsed, unification in M4).
    #[serde(default)]
    pub features: BTreeMap<String, Vec<String>>,
    /// Per-target overrides (optional, M7+ cross-compile).
    #[serde(default)]
    pub target: BTreeMap<String, TargetOverride>,
    /// Explicit binary targets (optional; defaults to `src/main.cpp` convention).
    #[serde(default)]
    pub bin: Vec<BinTarget>,
    /// Library target (optional).
    #[serde(default)]
    pub lib: Option<LibTarget>,
    /// Build profiles (optional).
    #[serde(default)]
    pub profile: Profiles,
    /// Workspace members (optional, M2+).
    #[serde(default)]
    pub workspace: Option<Workspace>,
    /// Patch overrides (optional, M4+; parsed in M1 so typos are caught).
    #[serde(default)]
    pub patch: BTreeMap<String, Dependency>,
    /// Directory containing `rivet.json` (not serialized).
    #[serde(skip)]
    pub dir: PathBuf,
}

/// Package metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    /// Package name (`[a-zA-Z0-9_-]+`).
    pub name: String,
    /// Version (`major.minor.patch`, SemVer 2.0).
    pub version: String,
    /// C++ standard, e.g. `c++20`. Defaults to `c++17`.
    #[serde(default)]
    pub standard: Option<String>,
}

/// A dependency: either a registry version string (`"11"`) or a detail object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Dependency {
    /// Registry dependency with a semver requirement, e.g. `"fmt": "11"`.
    Simple(String),
    /// Path / git / registry dependency with options.
    Detailed(DependencyDetail),
}

/// Detailed dependency entry.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DependencyDetail {
    /// Registry version requirement (alternative to `path`/`git`).
    #[serde(default)]
    pub version: Option<String>,
    /// Local path dependency.
    #[serde(default)]
    pub path: Option<String>,
    /// Git URL dependency.
    #[serde(default)]
    pub git: Option<String>,
    /// Git tag (mutually exclusive with `branch`/`rev`).
    #[serde(default)]
    pub tag: Option<String>,
    /// Git branch (mutually exclusive with `tag`/`rev`).
    #[serde(default)]
    pub branch: Option<String>,
    /// Exact git SHA (mutually exclusive with `tag`/`branch`).
    #[serde(default)]
    pub rev: Option<String>,
    /// Subdirectory of a git repo containing the package.
    #[serde(default)]
    pub subdir: Option<String>,
    /// Whether to fetch git submodules.
    #[serde(default)]
    pub submodules: Option<bool>,
    /// Inline package block for git deps without their own `rivet.json` (M4+).
    #[serde(default)]
    pub package: Option<InlinePackage>,
    /// System-library fallback (M6+).
    #[serde(default)]
    pub system: Option<bool>,
}

/// Inline package metadata for schemaless git deps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlinePackage {
    /// Package name.
    pub name: String,
    /// Package version.
    #[serde(default)]
    pub version: Option<String>,
}

/// Per-target dependency overrides.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TargetOverride {
    /// Target-specific dependencies.
    #[serde(default)]
    pub dependencies: BTreeMap<String, Dependency>,
}

/// A binary target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinTarget {
    /// Binary name.
    pub name: String,
    /// Path to the entry-point source, relative to the package root.
    pub path: String,
}

/// Library target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibTarget {
    /// `static` | `shared` | `header-only`.
    #[serde(rename = "type", default = "default_lib_type")]
    pub lib_type: String,
    /// Library name (defaults to package name).
    #[serde(default)]
    pub name: Option<String>,
    /// Entry source (defaults to `src/lib.cpp` convention).
    #[serde(default)]
    pub path: Option<String>,
}

fn default_lib_type() -> String {
    "static".to_string()
}

/// Build profiles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profiles {
    /// Dev profile.
    #[serde(default = "default_dev_profile")]
    pub dev: Profile,
    /// Release profile.
    #[serde(default = "default_release_profile")]
    pub release: Profile,
}

impl Default for Profiles {
    fn default() -> Self {
        Self {
            dev: default_dev_profile(),
            release: default_release_profile(),
        }
    }
}

/// One build profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// Optimization level 0–3.
    #[serde(default)]
    pub opt: u8,
    /// Debug info.
    #[serde(default = "default_true")]
    pub debug: bool,
    /// Link-time optimization.
    #[serde(default)]
    pub lto: bool,
    /// Sanitizers, e.g. `["address"]` (M7+; parsed now).
    #[serde(default)]
    pub sanitizers: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn default_dev_profile() -> Profile {
    Profile {
        opt: 0,
        debug: true,
        lto: false,
        sanitizers: Vec::new(),
    }
}

fn default_release_profile() -> Profile {
    Profile {
        opt: 3,
        debug: false,
        lto: false,
        sanitizers: Vec::new(),
    }
}

/// Workspace definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// Member globs, e.g. `["crates/*"]`.
    #[serde(default)]
    pub members: Vec<String>,
}

// ---------------------------------------------------------------------------
// Parsing + validation
// ---------------------------------------------------------------------------

const KNOWN_STANDARDS: &[&str] = &["c++11", "c++14", "c++17", "c++20", "c++23", "c++26"];
const KNOWN_LIB_TYPES: &[&str] = &["static", "shared", "header-only"];

/// Parse a manifest from JSONC source text.
pub fn parse_str(text: &str) -> Result<Manifest, Error> {
    let stripped = strip_jsonc(text);
    let value: serde_json::Value = serde_json::from_str(&stripped).map_err(|e| {
        Error::parse(
            "$",
            format!("invalid JSON: {e}. Hint: check for missing quotes, brackets, or commas"),
            e.line(),
            e.column(),
        )
    })?;
    validate_value(&value, &stripped)?;
    let mut manifest: Manifest = serde_json::from_value(value).map_err(|e| {
        // from_value loses spans; locate the offending key best-effort.
        let msg = e.to_string();
        let path = guess_path(&msg);
        let (line, column) = locate_key(&stripped, &path);
        Error::parse(path, format!("invalid manifest: {msg}"), line, column)
    })?;
    manifest.dir = PathBuf::new();
    Ok(manifest)
}

/// Load `rivet.json` from the directory `dir` (no upward search).
pub fn load(dir: &Path) -> Result<Manifest, Error> {
    let file = dir.join("rivet.json");
    let text = std::fs::read_to_string(&file).map_err(|e| Error::Io {
        path: file.display().to_string(),
        message: format!(
            "could not read rivet.json ({e}). Hint: run `rivet new <name>` or `rivet init` first"
        ),
        source: e,
    })?;
    let mut manifest = parse_str(&text).map_err(|e| match e {
        Error::Parse {
            path,
            message,
            line,
            column,
        } => Error::Parse {
            path,
            message: format!("{}: {}", file.display(), message),
            line,
            column,
        },
        other => other,
    })?;
    manifest.dir = dir.to_path_buf();
    Ok(manifest)
}

/// Search `start` and its ancestors for `rivet.json`; returns the directory.
pub fn find_manifest_dir(start: &Path) -> Result<PathBuf, Error> {
    let mut dir = if start.is_file() {
        start.parent().map(Path::to_path_buf)
    } else {
        Some(start.to_path_buf())
    };
    while let Some(d) = dir {
        if d.join("rivet.json").is_file() {
            return Ok(d);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    Err(Error::parse(
        "$",
        format!(
            "no rivet.json found from {}. Hint: run `rivet new <name>` or `rivet init`",
            start.display()
        ),
        1,
        1,
    ))
}

fn guess_path(msg: &str) -> String {
    // serde errors look like "missing field `name` at line 1 column 40".
    // Extract the first backquoted identifier as a hint.
    if let Some(a) = msg.find('`') {
        if let Some(b) = msg[a + 1..].find('`') {
            return msg[a + 1..a + 1 + b].to_string();
        }
    }
    "$".to_string()
}

fn validate_value(value: &serde_json::Value, stripped: &str) -> Result<(), Error> {
    let loc = |path: &str| locate_key(stripped, path);
    let err = |path: &str, message: String| {
        let (line, column) = loc(path);
        Error::parse(path, message, line, column)
    };

    let obj = value.as_object().ok_or_else(|| {
        err(
            "$",
            "manifest must be a JSON object `{ ... }`. Hint: run `rivet init` to regenerate"
                .to_string(),
        )
    })?;

    // package (required object with name+version).
    let pkg = obj.get("package").ok_or_else(|| {
        err(
            "package",
            "missing `package` table. Hint: add `\"package\": { \"name\": \"...\", \"version\": \"0.1.0\" }`".to_string(),
        )
    })?;
    let pkg_obj = pkg.as_object().ok_or_else(|| {
        err("package", "package must be an object. Hint: `\"package\": { \"name\": \"...\", \"version\": \"0.1.0\" }`".to_string())
    })?;
    let name = pkg_obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return Err(err(
            "package.name",
            "missing or empty `package.name`. Hint: use lowercase alphanumeric with `-`/`_`, e.g. `\"myapp\"`".to_string(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(err(
            "package.name",
            format!("invalid package name `{name}`. Hint: use only [a-zA-Z0-9_-]"),
        ));
    }
    let version = pkg_obj
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if version.is_empty() {
        return Err(err(
            "package.version",
            "missing `package.version`. Hint: add `\"version\": \"0.1.0\"`".to_string(),
        ));
    }
    if let Err(hint) = check_semver(version) {
        return Err(err(
            "package.version",
            format!("invalid version `{version}`: {hint}"),
        ));
    }
    if let Some(std) = pkg_obj.get("standard").and_then(|v| v.as_str()) {
        if !KNOWN_STANDARDS.contains(&std) {
            return Err(err(
                "package.standard",
                format!(
                    "unknown standard `{std}`. Hint: use one of {}",
                    KNOWN_STANDARDS.join(", ")
                ),
            ));
        }
    } else if pkg_obj.contains_key("standard") {
        return Err(err(
            "package.standard",
            "standard must be a string. Hint: use e.g. `\"standard\": \"c++20\"`".to_string(),
        ));
    }

    // dependencies (string or object; git ref exclusivity).
    for section in [
        "dependencies",
        "devDependencies",
        "buildDependencies",
        "patch",
    ] {
        if let Some(deps) = obj.get(section) {
            let map = deps.as_object().ok_or_else(|| {
                err(
                    section,
                    format!("`{section}` must be an object mapping names to deps"),
                )
            })?;
            for (dep_name, dep) in map {
                let path = format!("{section}.{dep_name}");
                validate_dependency(dep, &path, stripped)?;
            }
        }
    }

    // bin entries.
    if let Some(bins) = obj.get("bin") {
        let arr = bins.as_array().ok_or_else(|| {
            err("bin", "`bin` must be an array. Hint: `\"bin\": [{ \"name\": \"myapp\", \"path\": \"src/main.cpp\" }]`".to_string())
        })?;
        for (i, b) in arr.iter().enumerate() {
            let path = format!("bin[{i}]");
            let o = b.as_object().ok_or_else(|| {
                err(
                    &path,
                    "each `bin` entry must be an object with `name` and `path`".to_string(),
                )
            })?;
            if o.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .is_empty()
            {
                return Err(err(
                    &format!("{path}.name"),
                    "missing `name` in `bin` entry. Hint: add `\"name\": \"myapp\"`".to_string(),
                ));
            }
            if o.get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .is_empty()
            {
                return Err(err(
                    &format!("{path}.path"),
                    "missing `path` in `bin` entry. Hint: add `\"path\": \"src/main.cpp\"`"
                        .to_string(),
                ));
            }
        }
    }

    // lib type.
    if let Some(lib) = obj.get("lib") {
        let o = lib.as_object().ok_or_else(|| {
            err(
                "lib",
                "`lib` must be an object. Hint: `\"lib\": { \"type\": \"static\" }`".to_string(),
            )
        })?;
        if let Some(t) = o.get("type") {
            let s = t
                .as_str()
                .ok_or_else(|| err("lib.type", "`lib.type` must be a string".to_string()))?;
            if !KNOWN_LIB_TYPES.contains(&s) {
                return Err(err(
                    "lib.type",
                    format!(
                        "unknown lib type `{s}`. Hint: use one of {}",
                        KNOWN_LIB_TYPES.join(", ")
                    ),
                ));
            }
        }
    }

    // profiles: opt 0..=3.
    if let Some(profiles) = obj.get("profile") {
        let o = profiles
            .as_object()
            .ok_or_else(|| err("profile", "`profile` must be an object".to_string()))?;
        for section in ["dev", "release"] {
            if let Some(p) = o.get(section) {
                let po = p.as_object().ok_or_else(|| {
                    err(
                        &format!("profile.{section}"),
                        format!("`profile.{section}` must be an object"),
                    )
                })?;
                if let Some(opt) = po.get("opt") {
                    let ok = opt.as_u64().is_some_and(|v| v <= 3);
                    if !ok {
                        return Err(err(
                            &format!("profile.{section}.opt"),
                            "`opt` must be 0, 1, 2, or 3. Hint: dev=0, release=3".to_string(),
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

fn validate_dependency(dep: &serde_json::Value, path: &str, stripped: &str) -> Result<(), Error> {
    let loc = |p: &str| locate_key(stripped, p);
    let err = |p: &str, message: String| {
        let (line, column) = loc(p);
        Error::parse(p, message, line, column)
    };
    if let Some(s) = dep.as_str() {
        if s.is_empty() {
            return Err(err(
                path,
                "dependency version must not be empty. Hint: use e.g. `\"11\"` or `\"^1.2\"`"
                    .to_string(),
            ));
        }
        return Ok(());
    }
    let o = dep.as_object().ok_or_else(|| {
        err(
            path,
            "dependency must be a version string or an object. Hint: `\"fmt\": \"11\"` or `\"z\": { \"git\": \"...\", \"tag\": \"...\" }`".to_string(),
        )
    })?;
    let has_git = o.contains_key("git");
    let has_path = o.contains_key("path");
    let has_version = o.contains_key("version");
    if !has_git && !has_path && !has_version {
        return Err(err(
            path,
            "dependency object needs one of `version`, `path`, or `git`. Hint: add e.g. `\"version\": \"11\"`".to_string(),
        ));
    }
    let refs = ["tag", "branch", "rev"]
        .iter()
        .filter(|k| o.contains_key(**k))
        .count();
    if refs > 1 {
        return Err(err(
            &format!("{path}.rev"),
            "`tag`, `branch` and `rev` are mutually exclusive; specify exactly one. Hint: prefer `tag` for releases, `rev` for pins".to_string(),
        ));
    }
    if refs == 1 && !has_git {
        return Err(err(
            path,
            "`tag`/`branch`/`rev` require a `git` URL on the same entry".to_string(),
        ));
    }
    if let Some(git) = o.get("git").and_then(|v| v.as_str()) {
        if !(git.starts_with("https://")
            || git.starts_with("ssh://")
            || git.starts_with("git@")
            || git.starts_with("file://"))
        {
            // git@github.com:… (scp-like ssh) is accepted for now with a nudge.
            if !git.contains('@') {
                return Err(err(
                    &format!("{path}.git"),
                    format!("unsupported git URL scheme in `{git}`. Hint: use https://, ssh://, git@, or file://"),
                ));
            }
        }
    }
    Ok(())
}

/// Lightweight SemVer 2.0 shape check (`major.minor.patch[-pre][+build]`).
fn check_semver(version: &str) -> Result<(), String> {
    let without_build = version.split('+').next().unwrap_or("");
    if without_build.is_empty() {
        return Err("version must not be empty".to_string());
    }
    let core = without_build.split('-').next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return Err("expected `major.minor.patch`, e.g. `0.1.0`".to_string());
    }
    for p in &parts {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("`{p}` is not a non-negative integer"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Target discovery (zero-config conventions)
// ---------------------------------------------------------------------------

/// Resolve effective binary targets: explicit `bin` entries, else the
/// `src/main.cpp` convention (if the file exists).
pub fn effective_bins(manifest: &Manifest) -> Vec<BinTarget> {
    if !manifest.bin.is_empty() {
        return manifest.bin.clone();
    }
    let candidate = manifest.dir.join("src").join("main.cpp");
    if candidate.is_file() {
        vec![BinTarget {
            name: manifest.package.name.clone(),
            path: "src/main.cpp".to_string(),
        }]
    } else {
        Vec::new()
    }
}

/// Default C++ standard when `package.standard` is absent.
pub fn default_standard() -> &'static str {
    "c++17"
}

/// Effective C++ standard for a manifest.
pub fn effective_standard(manifest: &Manifest) -> &str {
    manifest
        .package
        .standard
        .as_deref()
        .unwrap_or(default_standard())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_line_and_block_comments() {
        let src = "{\n// hello\n\"a\": 1, /* x */ \"b\": 2,\n}";
        let out = strip_jsonc(src);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], 2);
    }

    #[test]
    fn preserves_line_numbers() {
        let src = "{\n// c1\n// c2\n\"a\": 1\n}";
        let out = strip_jsonc(src);
        assert_eq!(src.lines().count(), out.lines().count());
        // `"a"` must be on the same line in both.
        let orig_line = src.lines().position(|l| l.contains("\"a\"")).unwrap();
        let new_line = out.lines().position(|l| l.contains("\"a\"")).unwrap();
        assert_eq!(orig_line, new_line);
    }

    #[test]
    fn strips_trailing_commas() {
        let src = "{\"a\": [1, 2,], \"b\": {\"c\": 1,},}";
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(src)).unwrap();
        assert_eq!(v["a"][1], 2);
    }

    #[test]
    fn does_not_strip_inside_strings() {
        let src = "{\"a\": \"x /* not comment */ , // y\"}";
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(src)).unwrap();
        assert_eq!(v["a"], "x /* not comment */ , // y");
    }

    #[test]
    fn parses_full_example_manifest() {
        let src = r#"{
          "$schema": "https://rivet.dev/schema/rivet.json",
          "package": { "name": "myapp", "version": "0.1.0", "standard": "c++20" },
          "dependencies": {
            "fmt": "11",
            "mylib": { "path": "../mylib" },
            "zlib": { "git": "https://github.com/madler/zlib.git", "tag": "v1.3.1" }
          },
          "bin": [{ "name": "myapp", "path": "src/main.cpp" }],
          "lib": { "type": "static" },
        }"#;
        let m = parse_str(src).unwrap();
        assert_eq!(m.package.name, "myapp");
        assert_eq!(m.package.standard.as_deref(), Some("c++20"));
        assert_eq!(m.dependencies.len(), 3);
        assert_eq!(m.bin.len(), 1);
    }

    #[test]
    fn missing_package_reports_path_and_span() {
        let err = parse_str("{}").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("package"), "got: {msg}");
        assert!(msg.contains("at "), "got: {msg}");
    }

    #[test]
    fn git_refs_are_mutually_exclusive() {
        let src = r#"{
          "package": { "name": "a", "version": "0.1.0" },
          "dependencies": {
            "z": { "git": "https://example.com/z.git", "tag": "v1", "branch": "main" }
          }
        }"#;
        let err = parse_str(src).unwrap_err().to_string();
        assert!(err.contains("mutually exclusive"), "got: {err}");
        assert!(err.contains("dependencies.z"), "got: {err}");
    }

    #[test]
    fn bad_version_reports_json_path() {
        let src = "{\"package\": { \"name\": \"a\", \"version\": \"nope\" }}";
        let err = parse_str(src).unwrap_err().to_string();
        assert!(err.contains("package.version"), "got: {err}");
    }

    #[test]
    fn bad_standard_reports_hint() {
        let src =
            "{\"package\": { \"name\": \"a\", \"version\": \"0.1.0\", \"standard\": \"c++99\" }}";
        let err = parse_str(src).unwrap_err().to_string();
        assert!(err.contains("package.standard"), "got: {err}");
        assert!(err.contains("c++20"), "got: {err}");
    }

    #[test]
    fn syntax_error_reports_line_column() {
        let src = "{\n\"package\": {\n";
        let err = parse_str(src).unwrap_err().to_string();
        assert!(err.contains("at "), "got: {err}");
    }
}
