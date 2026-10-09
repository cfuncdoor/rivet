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

    /// A semantic (multi-file) error that has a JSON path but no precise
    /// source span. Points at 1:1 with the path attached.
    pub fn path_error(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self::parse(path, message, 1, 1)
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
    /// Package metadata (required, unless this is a virtual workspace root
    /// that only carries `workspace` + `patch`).
    #[serde(default)]
    pub package: Option<Package>,
    /// Runtime dependencies (optional).
    #[serde(default)]
    pub dependencies: BTreeMap<String, Dependency>,
    /// Dev dependencies (optional; used by `rivet test`).
    #[serde(default, rename = "devDependencies")]
    pub dev_dependencies: BTreeMap<String, Dependency>,
    /// Build dependencies (optional; used by build scripts).
    #[serde(default, rename = "buildDependencies")]
    pub build_dependencies: BTreeMap<String, Dependency>,
    /// Feature flags (optional; parsed, unified at resolve time).
    #[serde(default)]
    pub features: BTreeMap<String, Vec<String>>,
    /// Per-target overrides (optional; used for cross-compilation).
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
    /// Workspace members (optional).
    #[serde(default)]
    pub workspace: Option<Workspace>,
    /// Patch overrides (optional; parsed early so typos are caught).
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
    /// Path / git / registry dependency with options (boxed: the detail
    /// struct dwarfs the inline string variant).
    Detailed(Box<DependencyDetail>),
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
    /// Whether this dependency is optional, enabled via `features`
    /// (`dep:<name>` entries). Optional deps are skipped unless activated.
    /// Contract C1 — frozen.
    #[serde(default)]
    pub optional: Option<bool>,
    /// Inline package block for git deps without their own `rivet.json`.
    #[serde(default)]
    pub package: Option<InlinePackage>,
    /// System-library fallback.
    #[serde(default)]
    pub system: Option<bool>,
    /// Alternate registry URL for registry deps (default registry when
    /// absent). Contract C4 — frozen.
    #[serde(default)]
    pub registry: Option<String>,
    /// Foreign-package adapter: how to build a non-rivet dependency.
    /// Contract C5 — frozen.
    #[serde(default)]
    pub adapter: Option<AdapterSpec>,
}

/// Foreign-package build adapter (contract C5 — frozen).
///
/// Applies to git/path deps whose sources are not rivet packages (e.g. a
/// CMake-only library), or with `"system": true` to probe an installed
/// system library instead of building anything.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AdapterSpec {
    /// Adapter tool: `cmake`, `meson`, `make`, `pkg-config`, or `system`.
    #[serde(default)]
    pub tool: String,
    /// Build targets (meaning is tool-specific; empty = tool default).
    #[serde(default)]
    pub targets: Vec<String>,
    /// Tool-specific options (e.g. CMake `-D` definitions without the `-D`).
    #[serde(default)]
    pub options: BTreeMap<String, String>,
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
    /// Sanitizers, e.g. `["address"]`.
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

fn validate_package_table(
    pkg: &serde_json::Value,
    err: &impl Fn(&str, String) -> Error,
) -> Result<(), Error> {
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
    Ok(())
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

    // package (required, unless this is a virtual workspace root).
    let has_workspace = obj.get("workspace").and_then(|v| v.as_object()).is_some();
    let pkg = match obj.get("package") {
        Some(p) => Some(p),
        None if has_workspace => None,
        None => {
            return Err(err(
                "package",
                "missing `package` table. Hint: add `\"package\": { \"name\": \"...\", \"version\": \"0.1.0\" }`".to_string(),
            ));
        }
    };
    // A virtual workspace root carries no targets of its own.
    if pkg.is_none() {
        if obj.get("bin").is_some() {
            return Err(err(
                "bin",
                "virtual workspace manifest (no `package`) must not define `bin`. Hint: move targets into member packages".to_string(),
            ));
        }
        if obj.get("lib").is_some() {
            return Err(err(
                "lib",
                "virtual workspace manifest (no `package`) must not define `lib`. Hint: move targets into member packages".to_string(),
            ));
        }
    }
    if let Some(pkg) = pkg {
        validate_package_table(pkg, &err)?;
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
    if !has_git && !has_path && !has_version && !o.contains_key("system") {
        return Err(err(
            path,
            "dependency object needs one of `version`, `path`, or `git` (or `\"system\": true`). Hint: add e.g. `\"version\": \"11\"`".to_string(),
        ));
    }
    if o.get("system").and_then(|v| v.as_bool()).unwrap_or(false) && (has_path || has_git) {
        return Err(err(
            path,
            "`system: true` probes an installed library and takes no sources. Fix: drop `path`/`git` (or drop `system` to build the sources)".to_string(),
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
    if let Some(adapter) = o.get("adapter") {
        let obj = adapter.as_object().ok_or_else(|| {
            err(
                &format!("{path}.adapter"),
                "`adapter` must be an object. Hint: `\"adapter\": {\"tool\": \"cmake\"}`"
                    .to_string(),
            )
        })?;
        match obj.get("tool").and_then(|v| v.as_str()) {
            Some(t) if ["cmake", "meson", "make", "pkg-config", "system"].contains(&t) => {}
            Some(t) => {
                return Err(err(
                    &format!("{path}.adapter.tool"),
                    format!("unknown adapter tool `{t}`. Hint: use cmake, meson, make, pkg-config, or system"),
                ));
            }
            None => {
                return Err(err(
                    &format!("{path}.adapter.tool"),
                    "missing `adapter.tool`. Hint: add `\"tool\": \"cmake\"`".to_string(),
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

/// `true` when this manifest is a virtual workspace root (no `package`).
pub fn is_virtual(manifest: &Manifest) -> bool {
    manifest.package.is_none()
}

/// Package name, if present.
pub fn package_name(manifest: &Manifest) -> Option<&str> {
    manifest.package.as_ref().map(|p| p.name.as_str())
}

/// Package version, if present.
pub fn package_version(manifest: &Manifest) -> Option<&str> {
    manifest.package.as_ref().map(|p| p.version.as_str())
}

/// Resolve effective binary targets: explicit `bin` entries, else the
/// `src/main.cpp` convention (if the file exists).
///
/// Kept for API compatibility; new code prefers [`resolve_bins`] +
/// [`bin_full_sources`], which also partition helper sources.
pub fn effective_bins(manifest: &Manifest) -> Vec<BinTarget> {
    if !manifest.bin.is_empty() {
        return manifest.bin.clone();
    }
    let candidate = manifest.dir.join("src").join("main.cpp");
    if candidate.is_file() {
        if let Some(pkg) = &manifest.package {
            return vec![BinTarget {
                name: pkg.name.clone(),
                path: "src/main.cpp".to_string(),
            }];
        }
    }
    Vec::new()
}

/// `true` for C++ source extensions (`cpp cc cxx C`).
pub fn is_cpp_source(path: &Path) -> bool {
    path.extension()
        .and_then(|x| x.to_str())
        .is_some_and(|x| matches!(x, "cpp" | "cc" | "cxx" | "C"))
}

/// `true` for C/C++ header extensions.
pub fn is_header(path: &Path) -> bool {
    path.extension()
        .and_then(|x| x.to_str())
        .is_some_and(|x| matches!(x, "h" | "hpp" | "hxx" | "hh" | "H" | "tpp" | "ipp"))
}

/// All C++ sources under `<pkg>/src`, sorted, absolute.
pub fn all_src_cpps(pkg_dir: &Path) -> Vec<PathBuf> {
    let src = pkg_dir.join("src");
    if !src.is_dir() {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = walkdir::WalkDir::new(&src)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .filter(|p| is_cpp_source(p))
        .collect();
    out.sort();
    out
}

/// Collect source files for an entry path: a file → itself; a directory →
/// all C++ sources under it (recursive). Returns `None` when the entry does
/// not exist or a directory holds no C++ sources.
pub fn collect_sources(entry: &Path) -> Option<Vec<PathBuf>> {
    if entry.is_file() {
        return Some(vec![entry.to_path_buf()]);
    }
    if entry.is_dir() {
        let mut out: Vec<PathBuf> = walkdir::WalkDir::new(entry)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().to_path_buf())
            .filter(|p| is_cpp_source(p))
            .collect();
        out.sort();
        if out.is_empty() {
            return None;
        }
        return Some(out);
    }
    None
}

/// A binary target with its entry point resolved to an absolute path.
#[derive(Debug, Clone)]
pub struct ResolvedBin {
    /// Binary name.
    pub name: String,
    /// Absolute entry path (file or directory, as written in `bin.path`).
    pub entry: PathBuf,
    /// `true` when the entry is a directory (whole-directory semantics).
    pub is_dir: bool,
}

/// Resolve binary targets: explicit `bin` entries, else the `src/main.cpp`
/// convention. Returns an empty vec when the package has no bins.
pub fn resolve_bins(manifest: &Manifest) -> Vec<ResolvedBin> {
    if !manifest.bin.is_empty() {
        return manifest
            .bin
            .iter()
            .map(|b| {
                let entry = manifest.dir.join(&b.path);
                let is_dir = entry.is_dir();
                ResolvedBin {
                    name: b.name.clone(),
                    entry,
                    is_dir,
                }
            })
            .collect();
    }
    let candidate = manifest.dir.join("src").join("main.cpp");
    if candidate.is_file() {
        if let Some(pkg) = &manifest.package {
            return vec![ResolvedBin {
                name: pkg.name.clone(),
                entry: candidate,
                is_dir: false,
            }];
        }
    }
    Vec::new()
}

/// A library target with sources resolved to absolute paths.
#[derive(Debug, Clone)]
pub struct ResolvedLib {
    /// Library name (explicit `lib.name`, else the package name).
    pub name: String,
    /// `static` | `shared` | `header-only` (as written; `shared` is rejected
    /// at build time — it needs an export story that does not exist yet).
    pub lib_type: String,
    /// Absolute root sources (empty for header-only).
    pub roots: Vec<PathBuf>,
    /// Public include dir, when `<pkg>/include` exists.
    pub include_dir: Option<PathBuf>,
}

/// `true` when the resolved lib carries compiled sources.
pub fn lib_has_sources(lib: &ResolvedLib) -> bool {
    !lib.roots.is_empty()
}

/// Resolve the library target.
///
/// Precedence: explicit `lib` first; then `src/lib.cpp`; then — only when
/// the package has no bins — a static lib from leftover `src/` sources, or
/// a header-only lib from `include/` headers. A package WITH bins and an
/// `include/` dir is a bin package (headers belong to its helpers), not a
/// header-only lib: otherwise every multi-file bin would lose its helpers.
/// Returns `Ok(None)` when the package has no lib.
pub fn resolve_lib(manifest: &Manifest) -> Result<Option<ResolvedLib>, Error> {
    let include_dir = {
        let inc = manifest.dir.join("include");
        inc.is_dir().then_some(inc)
    };
    let package_name = manifest.package.as_ref().map(|p| p.name.clone());
    if let Some(lib) = &manifest.lib {
        let name = lib.name.clone().or(package_name).ok_or_else(|| {
            Error::path_error(
                "lib.name",
                "virtual manifest `lib` needs an explicit `name`. Hint: add `\"name\": \"...\"`",
            )
        })?;
        if let Some(path) = &lib.path {
            let entry = manifest.dir.join(path);
            let roots = collect_sources(&entry).ok_or_else(|| {
                Error::path_error(
                    "lib.path",
                    format!(
                        "lib source not found: {}. Fix: create it or fix `lib.path` in rivet.json",
                        entry.display()
                    ),
                )
            })?;
            return Ok(Some(ResolvedLib {
                name,
                lib_type: lib.lib_type.clone(),
                roots,
                include_dir,
            }));
        }
        // No explicit path: `src/lib.cpp` convention, else header-only rules.
        let candidate = manifest.dir.join("src").join("lib.cpp");
        if candidate.is_file() {
            return Ok(Some(ResolvedLib {
                name,
                lib_type: lib.lib_type.clone(),
                roots: vec![candidate],
                include_dir,
            }));
        }
        if lib.lib_type == "header-only" {
            return Ok(Some(ResolvedLib {
                name,
                lib_type: lib.lib_type.clone(),
                roots: Vec::new(),
                include_dir,
            }));
        }
        if lib.lib_type == "shared" || lib.lib_type == "static" {
            return Err(Error::path_error(
                "lib.path",
                "lib has no sources: src/lib.cpp is missing. Fix: create src/lib.cpp or set `lib.path`".to_string(),
            ));
        }
        // Unreachable: `lib.type` is validated at parse time.
        return Err(Error::path_error(
            "lib.type",
            "unknown lib type. Hint: use static, shared, or header-only".to_string(),
        ));
    }
    // Convention: `src/lib.cpp` → static lib. Without bins, leftover
    // `src/` sources form a static lib, else `include/` headers form a
    // header-only lib. With bins, there is no lib (see doc comment).
    let candidate = manifest.dir.join("src").join("lib.cpp");
    if candidate.is_file() {
        if let Some(name) = package_name {
            return Ok(Some(ResolvedLib {
                name,
                lib_type: "static".to_string(),
                roots: vec![candidate],
                include_dir,
            }));
        }
    }
    if resolve_bins(manifest).is_empty() {
        let srcs = all_src_cpps(&manifest.dir);
        if !srcs.is_empty() {
            if let Some(name) = package_name {
                return Ok(Some(ResolvedLib {
                    name,
                    lib_type: "static".to_string(),
                    roots: srcs,
                    include_dir,
                }));
            }
        }
        if let Some(inc) = include_dir {
            if has_headers(&inc) {
                if let Some(name) = package_name {
                    return Ok(Some(ResolvedLib {
                        name,
                        lib_type: "header-only".to_string(),
                        roots: Vec::new(),
                        include_dir: Some(inc),
                    }));
                }
            }
        }
    }
    Ok(None)
}

/// `true` when `dir` (recursively) holds C/C++ headers.
fn has_headers(dir: &Path) -> bool {
    walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .any(|p| is_header(&p))
}

/// Full source list for a binary target.
///
/// Directory entries keep whole-directory semantics. File entries compile
/// the entry plus every helper (`src/**/*.cpp` minus all target roots) when
/// the package has no lib; with a lib, the bin is just its entry file and
/// links the archive instead.
pub fn bin_full_sources(
    manifest: &Manifest,
    bin: &ResolvedBin,
    lib: &Option<ResolvedLib>,
) -> Result<Vec<PathBuf>, Error> {
    if bin.is_dir {
        return collect_sources(&bin.entry).ok_or_else(|| {
            Error::path_error(
                format!("bin.{}", bin.name),
                format!(
                    "bin source not found: {}. Fix: create it or fix the `bin` path in rivet.json",
                    bin.entry.display()
                ),
            )
        });
    }
    if !bin.entry.is_file() {
        return Err(Error::path_error(
            format!("bin.{}", bin.name),
            format!(
                "bin source not found: {}. Fix: create it or fix the `bin` path in rivet.json",
                bin.entry.display()
            ),
        ));
    }
    if let Some(lib) = lib {
        if lib.roots.iter().any(|r| r == &bin.entry) {
            return Err(Error::path_error(
                format!("bin.{}", bin.name),
                format!(
                    "bin entry {} is also a lib source. Fix: use a separate entry file (e.g. src/main.cpp) and keep lib sources in src/lib.cpp + helpers",
                    bin.entry.display()
                ),
            ));
        }
        // The lib owns the helpers; the bin links its archive.
        return Ok(vec![bin.entry.clone()]);
    }
    // No lib: multi-file bin — entry plus all helpers.
    let mut bin_roots: Vec<PathBuf> = resolve_bins(manifest)
        .into_iter()
        .filter(|b| !b.is_dir)
        .map(|b| b.entry)
        .collect();
    bin_roots.push(manifest.dir.join("src").join("lib.cpp"));
    let mut sources = vec![bin.entry.clone()];
    for cpp in all_src_cpps(&manifest.dir) {
        if cpp != bin.entry && !bin_roots.contains(&cpp) {
            sources.push(cpp);
        }
    }
    sources.sort();
    Ok(sources)
}

/// Full source list for a library target: roots plus helpers
/// (`src/**/*.cpp` minus all bin entry files).
pub fn lib_full_sources(manifest: &Manifest, lib: &ResolvedLib) -> Vec<PathBuf> {
    if !lib_has_sources(lib) {
        return Vec::new();
    }
    let bin_roots: Vec<PathBuf> = resolve_bins(manifest)
        .into_iter()
        .filter(|b| !b.is_dir)
        .map(|b| b.entry)
        .collect();
    let mut sources = lib.roots.clone();
    for cpp in all_src_cpps(&manifest.dir) {
        if !bin_roots.contains(&cpp) && !sources.contains(&cpp) {
            sources.push(cpp);
        }
    }
    sources.sort();
    sources
}

impl Dependency {
    /// The local path of a `{"path": ...}` dependency, if any.
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Detailed(d) => d.path.as_deref(),
            Self::Simple(_) => None,
        }
    }

    /// `true` when this dep is optional (activated via `features`).
    pub fn is_optional(&self) -> bool {
        match self {
            Self::Detailed(d) => d.optional.unwrap_or(false),
            Self::Simple(_) => false,
        }
    }

    /// `true` for registry (`Simple("11")` / `{"version": ...}`) entries.
    pub fn is_registry(&self) -> bool {
        match self {
            Self::Simple(_) => true,
            Self::Detailed(d) => d.version.is_some() && d.path.is_none() && d.git.is_none(),
        }
    }

    /// `true` for git (`{"git": ...}`) entries.
    pub fn is_git(&self) -> bool {
        match self {
            Self::Detailed(d) => d.git.is_some(),
            Self::Simple(_) => false,
        }
    }
}

/// `(name, absolute dir)` for every `dependencies` entry with a `path`.
/// Only `dependencies` are built (`devDependencies` feed `rivet test`,
/// `buildDependencies` feed build scripts).
pub fn path_dependencies(manifest: &Manifest) -> Vec<(String, PathBuf)> {
    manifest
        .dependencies
        .iter()
        .filter_map(|(name, dep)| dep.path().map(|p| (name.clone(), manifest.dir.join(p))))
        .collect()
}

// ---------------------------------------------------------------------------
// Workspace member expansion (hand-rolled globs, no new deps)
// ---------------------------------------------------------------------------

/// Match `name` against a single path-segment pattern (`*` = any run,
/// `?` = one char).
fn match_segment(pattern: &str, name: &str) -> bool {
    let (px, nx) = (pattern.as_bytes(), name.as_bytes());
    let (mut star, mut mark) = (None::<usize>, 0usize);
    let (mut i, mut j) = (0usize, 0usize);
    while j < nx.len() {
        if i < px.len() && (px[i] == b'?' || px[i] == nx[j]) {
            i += 1;
            j += 1;
        } else if i < px.len() && px[i] == b'*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    while i < px.len() && px[i] == b'*' {
        i += 1;
    }
    i == px.len()
}

/// Match a `/`-separated relative path against a glob pattern with segments
/// of literals/`*`/`?`, plus `**` (any number of segments, including zero).
pub fn match_glob(pattern: &str, rel: &str) -> bool {
    fn rec(pats: &[&str], segs: &[&str]) -> bool {
        if pats.is_empty() {
            return segs.is_empty();
        }
        if pats[0] == "**" {
            return (0..=segs.len()).any(|k| rec(&pats[1..], &segs[k..]));
        }
        if segs.is_empty() {
            return false;
        }
        match_segment(pats[0], segs[0]) && rec(&pats[1..], &segs[1..])
    }
    let pats: Vec<&str> = pattern.split('/').collect();
    let segs: Vec<&str> = rel.split('/').collect();
    rec(&pats, &segs)
}

/// Expand workspace member globs to sorted, deduped absolute package dirs.
///
/// Every match must be a directory containing `rivet.json`; a pattern with
/// zero matches is an error (usually a typo).
pub fn expand_members(root: &Path, members: &[String]) -> Result<Vec<PathBuf>, Error> {
    let mut out: Vec<PathBuf> = Vec::new();
    for pattern in members {
        if pattern.is_empty() {
            return Err(Error::path_error(
                "workspace.members",
                "workspace member pattern must not be empty".to_string(),
            ));
        }
        let has_magic = pattern.contains('*') || pattern.contains('?');
        if !has_magic {
            let dir = root.join(pattern);
            if !dir.is_dir() {
                return Err(Error::path_error(
                    "workspace.members",
                    format!(
                        "workspace member `{pattern}` not found under {}. Fix: create it or fix `workspace.members`",
                        root.display()
                    ),
                ));
            }
            if !dir.join("rivet.json").is_file() {
                return Err(Error::path_error(
                    "workspace.members",
                    format!(
                        "workspace member `{pattern}` has no rivet.json. Fix: run `rivet init` in {}",
                        dir.display()
                    ),
                ));
            }
            out.push(dir);
            continue;
        }
        let mut hits: Vec<PathBuf> = walkdir::WalkDir::new(root)
            .follow_links(false)
            .min_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_dir())
            .map(|e| e.path().to_path_buf())
            .filter(|p| {
                p.strip_prefix(root)
                    .ok()
                    .and_then(|rel| rel.to_str())
                    .map(|rel| match_glob(pattern, &rel.replace('\\', "/")))
                    .unwrap_or(false)
            })
            .collect();
        hits.sort();
        if hits.is_empty() {
            return Err(Error::path_error(
                "workspace.members",
                format!(
                    "workspace pattern `{pattern}` matched nothing under {}. Fix: check `workspace.members`",
                    root.display()
                ),
            ));
        }
        for dir in hits {
            if !dir.join("rivet.json").is_file() {
                return Err(Error::path_error(
                    "workspace.members",
                    format!(
                        "workspace member `{}` has no rivet.json. Fix: run `rivet init` in {}",
                        dir.strip_prefix(root).unwrap_or(&dir).display(),
                        dir.display()
                    ),
                ));
            }
            out.push(dir);
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Default C++ standard when `package.standard` is absent.
pub fn default_standard() -> &'static str {
    "c++17"
}

/// Effective C++ standard for a manifest (virtual roots use the default).
pub fn effective_standard(manifest: &Manifest) -> &str {
    manifest
        .package
        .as_ref()
        .and_then(|p| p.standard.as_deref())
        .unwrap_or(default_standard())
}

/// Synthesize a manifest for a schemaless foreign dependency (adapter or
/// system source with no `rivet.json`).
///
/// The inline `package` block is required: its `name` must equal the dep
/// key and it must carry a `version` (there is nothing else to read one
/// from). The synthesized package is always a leaf (no dependencies of its
/// own). Used identically by the resolver and the build graph.
pub fn synthesize_manifest(
    dir: &Path,
    key: &str,
    inline: &Option<InlinePackage>,
) -> Result<Manifest, Error> {
    let inline = inline.as_ref().ok_or_else(|| {
        Error::path_error(
            format!("dependencies.{key}"),
            format!(
                "dep `{key}` has no rivet.json and no inline `package` block. Fix: add a rivet.json upstream or `\"package\": {{\"name\": \"{key}\", \"version\": \"0.1.0\"}}` to the dep entry"
            ),
        )
    })?;
    if inline.name != key {
        return Err(Error::path_error(
            format!("dependencies.{key}.package"),
            format!(
                "inline package name `{}` != dep key `{key}`. Fix: rename one of them",
                inline.name
            ),
        ));
    }
    let version = inline.version.clone().ok_or_else(|| {
        Error::path_error(
            format!("dependencies.{key}.package"),
            "inline `package` block has no version. Fix: add `\"version\": \"0.1.0\"`".to_string(),
        )
    })?;
    Ok(Manifest {
        schema: None,
        package: Some(Package {
            name: key.to_string(),
            version,
            standard: None,
        }),
        dependencies: BTreeMap::new(),
        dev_dependencies: BTreeMap::new(),
        build_dependencies: BTreeMap::new(),
        features: BTreeMap::new(),
        target: BTreeMap::new(),
        bin: Vec::new(),
        lib: None,
        profile: Profiles::default(),
        workspace: None,
        patch: BTreeMap::new(),
        dir: dir.to_path_buf(),
    })
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
        assert_eq!(m.package.as_ref().unwrap().name, "myapp");
        assert_eq!(
            m.package.as_ref().unwrap().standard.as_deref(),
            Some("c++20")
        );
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

    fn pkg_in(dir: &Path, json: &str) -> Manifest {
        std::fs::write(dir.join("rivet.json"), json).unwrap();
        let mut m = parse_str(json).unwrap();
        m.dir = dir.to_path_buf();
        m
    }

    #[test]
    fn virtual_manifest_parses_without_package() {
        let m = parse_str("{\"workspace\": {\"members\": [\"crates/*\"]}}").unwrap();
        assert!(is_virtual(&m));
        assert_eq!(package_name(&m), None);
        assert_eq!(effective_standard(&m), "c++17");
    }

    #[test]
    fn virtual_manifest_rejects_own_targets() {
        let err = parse_str(
            "{\"workspace\": {\"members\": []}, \"bin\": [{\"name\": \"a\", \"path\": \"x\"}]}",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("virtual"), "got: {err}");
    }

    #[test]
    fn lib_convention_from_src_lib() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.cpp"), "").unwrap();
        std::fs::write(dir.path().join("src/util.cpp"), "").unwrap();
        let m = pkg_in(
            dir.path(),
            "{\"package\":{\"name\":\"mylib\",\"version\":\"0.1.0\"}}",
        );
        let lib = resolve_lib(&m).unwrap().unwrap();
        assert_eq!(lib.name, "mylib");
        assert_eq!(lib.lib_type, "static");
        // Helpers belong to the lib.
        let sources = lib_full_sources(&m, &lib);
        assert_eq!(sources.len(), 2, "got: {sources:?}");
    }

    #[test]
    fn header_only_convention_from_include() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("include").join("my")).unwrap();
        std::fs::write(dir.path().join("include").join("my").join("a.h"), "").unwrap();
        let m = pkg_in(
            dir.path(),
            "{\"package\":{\"name\":\"hdr\",\"version\":\"0.1.0\"}}",
        );
        let lib = resolve_lib(&m).unwrap().unwrap();
        assert_eq!(lib.lib_type, "header-only");
        assert!(lib.roots.is_empty());
        assert!(lib.include_dir.is_some());
    }

    #[test]
    fn bins_win_over_include_headers() {
        // A bin package with headers in include/ is NOT a header-only lib:
        // the helpers belong to the bin.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("include")).unwrap();
        std::fs::write(dir.path().join("src/main.cpp"), "").unwrap();
        std::fs::write(dir.path().join("src/util.cpp"), "").unwrap();
        std::fs::write(dir.path().join("include/util.h"), "").unwrap();
        let m = pkg_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        assert!(resolve_lib(&m).unwrap().is_none());
        let bins = resolve_bins(&m);
        let sources = bin_full_sources(&m, &bins[0], &None).unwrap();
        assert_eq!(sources.len(), 2, "got: {sources:?}");
    }

    #[test]
    fn leftover_sources_form_lib_without_bins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/util.cpp"), "").unwrap();
        let m = pkg_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        let lib = resolve_lib(&m).unwrap().unwrap();
        assert_eq!(lib.lib_type, "static");
        assert_eq!(lib.roots.len(), 1);
    }

    #[test]
    fn no_lib_without_sources_or_headers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.cpp"), "").unwrap();
        let m = pkg_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        assert!(resolve_lib(&m).unwrap().is_none());
    }

    #[test]
    fn bin_gets_helpers_without_lib() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        for f in ["main.cpp", "util.cpp", "extra.cc"] {
            std::fs::write(dir.path().join("src").join(f), "").unwrap();
        }
        let m = pkg_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        let bins = resolve_bins(&m);
        assert_eq!(bins.len(), 1);
        let sources = bin_full_sources(&m, &bins[0], &None).unwrap();
        assert_eq!(sources.len(), 3, "got: {sources:?}");
    }

    #[test]
    fn bin_is_entry_only_with_lib() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        for f in ["main.cpp", "lib.cpp", "util.cpp"] {
            std::fs::write(dir.path().join("src").join(f), "").unwrap();
        }
        let m = pkg_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"}}",
        );
        let lib = resolve_lib(&m).unwrap();
        assert!(lib.is_some());
        let bins = resolve_bins(&m);
        let sources = bin_full_sources(&m, &bins[0], &lib).unwrap();
        assert_eq!(sources.len(), 1, "got: {sources:?}");
    }

    #[test]
    fn bin_lib_overlap_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.cpp"), "").unwrap();
        let m = pkg_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"bin\":[{\"name\":\"a\",\"path\":\"src/lib.cpp\"}]}",
        );
        let lib = resolve_lib(&m).unwrap();
        let bins = resolve_bins(&m);
        let err = bin_full_sources(&m, &bins[0], &lib)
            .unwrap_err()
            .to_string();
        assert!(err.contains("also a lib source"), "got: {err}");
    }

    #[test]
    fn glob_segments() {
        assert!(match_glob("crates/*", "crates/foo"));
        assert!(!match_glob("crates/*", "crates/foo/bar"));
        assert!(match_glob("crates/**", "crates/foo/bar"));
        assert!(match_glob("**", "a/b/c"));
        assert!(match_glob("src/?ain.cpp", "src/main.cpp"));
        assert!(!match_glob("src/*.cpp", "src/main.h"));
        assert!(match_glob("exact", "exact"));
    }

    #[test]
    fn member_expansion_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        for member in ["crates/app", "crates/lib"] {
            let d = dir.path().join(member);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("rivet.json"),
                "{\"package\":{\"name\":\"x\",\"version\":\"0.1.0\"}}",
            )
            .unwrap();
        }
        let got = expand_members(dir.path(), &[String::from("crates/*")]).unwrap();
        assert_eq!(got.len(), 2, "got: {got:?}");
        // Unmatched pattern is an error.
        let err = expand_members(dir.path(), &[String::from("nope/*")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("matched nothing"), "got: {err}");
        // Member without rivet.json is an error.
        std::fs::create_dir_all(dir.path().join("crates/bare")).unwrap();
        let err = expand_members(dir.path(), &[String::from("crates/*")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no rivet.json"), "got: {err}");
    }

    #[test]
    fn path_deps_listed() {
        let m = parse_str(
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"dependencies\":{\"mylib\":{\"path\":\"../mylib\"},\"fmt\":\"11\"}}",
        )
        .unwrap();
        let deps = path_dependencies(&m);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].0, "mylib");
    }

    #[test]
    fn system_with_sources_is_rejected() {
        let err = parse_str(
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"dependencies\":{\"z\":{\"git\":\"https://example.com/z.git\",\"system\":true}}}",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("takes no sources"), "got: {err}");
    }

    #[test]
    fn system_only_parses() {
        let m = parse_str(
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"dependencies\":{\"z\":{\"system\":true,\"version\":\"1.2\"}}}",
        )
        .unwrap();
        assert!(path_dependencies(&m).is_empty());
    }

    #[test]
    fn synthesize_needs_inline_block() {
        let dir = Path::new("/pkg/dep");
        let err = synthesize_manifest(dir, "dep", &None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("inline `package`"), "got: {err}");
        let inline = InlinePackage {
            name: "dep".to_string(),
            version: None,
        };
        let err = synthesize_manifest(dir, "dep", &Some(inline))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no version"), "got: {err}");
        let inline = InlinePackage {
            name: "other".to_string(),
            version: Some("1.0.0".to_string()),
        };
        let err = synthesize_manifest(dir, "dep", &Some(inline))
            .unwrap_err()
            .to_string();
        assert!(err.contains("!= dep key"), "got: {err}");
        let inline = InlinePackage {
            name: "dep".to_string(),
            version: Some("1.0.0".to_string()),
        };
        let m = synthesize_manifest(dir, "dep", &Some(inline)).unwrap();
        assert_eq!(package_name(&m), Some("dep"));
        assert!(m.dependencies.is_empty());
    }

    #[test]
    fn explicit_dir_bin_entry_collects() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tools").join("sub")).unwrap();
        std::fs::write(dir.path().join("tools").join("t.cpp"), "").unwrap();
        std::fs::write(dir.path().join("tools").join("sub").join("u.cxx"), "").unwrap();
        std::fs::write(dir.path().join("tools").join("skip.txt"), "").unwrap();
        let m = pkg_in(
            dir.path(),
            "{\"package\":{\"name\":\"a\",\"version\":\"0.1.0\"},\"bin\":[{\"name\":\"tool\",\"path\":\"tools\"}]}",
        );
        let bins = resolve_bins(&m);
        assert_eq!(bins.len(), 1);
        assert!(bins[0].is_dir);
        let sources = bin_full_sources(&m, &bins[0], &None).unwrap();
        assert_eq!(sources.len(), 2, "got: {sources:?}");
    }
}
