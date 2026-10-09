//! Foreign package adapters: build non-rivet sources (CMake, Meson, Make)
//! and expose their include dirs + libs, or probe installed system
//! libraries (`system: true`, pkg-config).
//!
//! This crate is a library only. The coordinator wires [`AdapterOutput`]
//! into the build graph after this lands; graph/CLI integration is
//! explicitly out of scope.
//!
//! # Tools
//!
//! | `AdapterSpec.tool` | behavior |
//! |---|---|
//! | `cmake` | configure (`cmake -S src -B build -DCMAKE_BUILD_TYPE=… -DCMAKE_INSTALL_PREFIX=<build>/install -Dk=v…`) then `cmake --build build --config … [--target t…] [--parallel N]`. See [`cmake`]. |
//! | `meson` | `meson setup` (fresh or `--reconfigure`) then `meson compile -C build [targets…]`. See [`meson`]. |
//! | `make` | `make -C src [targets…] [K=V…] [-j N]` (in-source). See [`make_`]. |
//! | `pkg-config` | strict `pkg-config --cflags/--libs <name>` probe, no sources. See [`system`]. |
//! | `system` | `pkg-config` when the binary exists (falling back to a well-known-dir search when the package is not known to pkg-config), else the dir search directly. See [`system`]. |
//!
//! Probing a foreign source never executes anything from that source except
//! the adapter tool itself (`cmake`, `meson`, `make`). No configure scripts,
//! no fetched code, no build helpers are run at probe time.
//!
//! # Integrator contract
//!
//! - [`build_adapter`] turns a foreign source dir + [`AdapterSpec`][rivet_manifest::AdapterSpec]
//!   into an [`AdapterOutput`]: `include_dirs` feed the dependent's compile
//!   include path (same semantics as `unit_inputs` include dirs), `libs`
//!   feed the link line — [`LibLink::File`] absolute archives first (static
//!   preferred), [`LibLink::Name`] as `-l`/import names — and `lib_dirs`
//!   feed the library search path (and DLL lookup at runtime).
//! - [`probe_installed`] does the same for sourceless (`system: true`) deps.
//! - [`fingerprint`] hashes (source dir contents + adapter spec + toolchain
//!   fingerprint) into an opaque string; the integrator stores it and calls
//!   [`needs_rebuild`] to detect staleness. Any source/target/option/
//!   toolchain change flips the fingerprint.
//! - Missing tools are [`Error::ToolMissing`] (actionable install hints),
//!   never silent fallbacks. A `targets` list that matches nothing built is
//!   [`Error::NoLibraries`], never a quiet "build everything instead".

#![forbid(unsafe_code)]

mod cmake;
mod discover;
mod fingerprint;
mod make;
mod meson;
mod system;
mod util;

use std::path::{Path, PathBuf};

use rivet_manifest::AdapterSpec;
use rivet_toolchain::CxxToolchain;
use thiserror::Error;

pub use discover::{discover_include_dirs, discover_libs, dll_dirs, lib_dirs_of};
pub use fingerprint::{fingerprint, needs_rebuild, source_dir_hash};
pub use system::{parse_pkg_config, probe_pkg_config, probe_system, search_well_known};
pub use util::tool_available;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Adapter errors. Every variant is actionable: what failed, where, and how
/// to fix it. Adapter failures never silently fall back to another tool.
#[derive(Debug, Error)]
pub enum Error {
    /// Unknown `adapter.tool` value.
    #[error(
        "unknown adapter tool `{tool}`. Fix: use one of cmake, meson, make, pkg-config, system"
    )]
    UnknownTool {
        /// The rejected tool string.
        tool: String,
    },
    /// The adapter spec is malformed or misapplied.
    #[error("{message}")]
    InvalidSpec {
        /// What is wrong + how to fix it.
        message: String,
    },
    /// A required external tool is not on `PATH`.
    #[error("`{tool}` not found on PATH. Fix: {hint}")]
    ToolMissing {
        /// Binary name that was looked up.
        tool: String,
        /// Install hint for this tool.
        hint: String,
    },
    /// An adapter subprocess exited nonzero (configure, build, or probe).
    #[error("failed to {what}:\n  command: {command}\n{output}\nFix: {hint}")]
    CommandFailed {
        /// What was attempted (e.g. `configure mini with cmake`).
        what: String,
        /// The full command line, for reproduction.
        command: String,
        /// Tail of combined stdout+stderr.
        output: String,
        /// How to fix it.
        hint: String,
    },
    /// A source build produced no linkable library (or none matching the
    /// requested targets).
    #[error("adapter build in `{dir}` produced no linkable library matching targets [{targets}]. Found: {found}. Fix: check the adapter `targets` list against the foreign project's target names, or leave `targets` empty to accept every built library")]
    NoLibraries {
        /// Build (or source, for make) dir that was scanned.
        dir: String,
        /// Requested targets, comma-joined (`<default>` when empty).
        targets: String,
        /// Discovered files, comma-joined (`<none>` when empty).
        found: String,
    },
    /// A `system`/`pkg-config` probe found no installed library.
    #[error("system library `{package}` not found. {detail}")]
    SystemNotFound {
        /// Package name that was probed.
        package: String,
        /// pkg-config output and/or dirs searched + install hint.
        detail: String,
    },
    /// Filesystem failure while hashing/scanning.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Output model (frozen for the integrator)
// ---------------------------------------------------------------------------

/// One link input from an adapter-built (or system-probed) dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibLink {
    /// Absolute path to a built archive/shared lib. Preferred: the
    /// integrator links these directly (no search path needed).
    File(PathBuf),
    /// Bare library name (`-l` name on GNU, import name on MSVC).
    /// Used for system libs reported by pkg-config and for shared libs
    /// without a usable absolute path.
    Name(String),
}

impl LibLink {
    /// `true` for [`LibLink::File`].
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File(_))
    }
}

impl std::fmt::Display for LibLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(p) => write!(f, "{}", p.display()),
            Self::Name(n) => write!(f, "{n}"),
        }
    }
}

/// What a foreign dependency contributes to a dependent's build.
///
/// Mirrors `rivet-graph` `UnitInputs` semantics: `include_dirs` join the
/// compile include path, `lib_dirs` join the library search path, and
/// `libs` join the link line in order (static archives first).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdapterOutput {
    /// Public include dirs (source `include/`, generated-header build dirs,
    /// or system include dirs). All entries exist at probe time.
    pub include_dirs: Vec<PathBuf>,
    /// Dirs holding [`LibLink::File`] entries (also DLL lookup dirs).
    pub lib_dirs: Vec<PathBuf>,
    /// Link inputs in link order: absolute files first (static preferred),
    /// then bare names.
    pub libs: Vec<LibLink>,
    /// Preprocessor defines (`NAME` or `NAME=value`, without `-D`).
    /// Populated by `pkg-config`/`system` probes; source builds report none.
    pub defines: Vec<String>,
}

impl AdapterOutput {
    /// `true` when there is nothing to feed the build (no libs, no
    /// includes, no defines).
    pub fn is_empty(&self) -> bool {
        self.libs.is_empty() && self.include_dirs.is_empty() && self.defines.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Adapter selection + build context
// ---------------------------------------------------------------------------

/// The foreign-build tool named by `AdapterSpec.tool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    /// `cmake`: configure + `cmake --build`.
    Cmake,
    /// `meson`: `meson setup` + `meson compile`.
    Meson,
    /// `make`: `make -C src` (in-source, best-effort).
    Make,
    /// `pkg-config`: strict installed-library probe.
    PkgConfig,
    /// `system`: pkg-config-then-dir-search installed-library probe.
    System,
}

impl AdapterKind {
    /// Map an `AdapterSpec.tool` string to its kind. Empty strings are an
    /// [`Error::InvalidSpec`]; anything else unknown is
    /// [`Error::UnknownTool`].
    pub fn from_tool(tool: &str) -> Result<Self, Error> {
        match tool {
            "cmake" => Ok(Self::Cmake),
            "meson" => Ok(Self::Meson),
            "make" => Ok(Self::Make),
            "pkg-config" => Ok(Self::PkgConfig),
            "system" => Ok(Self::System),
            "" => Err(Error::InvalidSpec {
                message: "missing `adapter.tool`. Fix: add `\"tool\": \"cmake\"` (or meson, make, pkg-config, system)".to_string(),
            }),
            other => Err(Error::UnknownTool {
                tool: other.to_string(),
            }),
        }
    }

    /// The canonical tool string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cmake => "cmake",
            Self::Meson => "meson",
            Self::Make => "make",
            Self::PkgConfig => "pkg-config",
            Self::System => "system",
        }
    }

    /// `true` for tools that build from sources (cmake/meson/make).
    pub fn builds_sources(self) -> bool {
        matches!(self, Self::Cmake | Self::Meson | Self::Make)
    }
}

/// Where and how a source-building adapter runs.
#[derive(Debug, Clone)]
pub struct AdapterCtx {
    /// Scratch build dir (created if missing). Must be writable; for
    /// immutable sources (git checkouts, vendor copies) the integrator
    /// passes a scratch copy's sibling dir, never the checkout itself —
    /// except `make`, which builds in-source by nature.
    pub build_dir: PathBuf,
    /// `Debug` or `Release` (maps to `CMAKE_BUILD_TYPE`, `--config`,
    /// and meson `--buildtype`; rivet `dev` → `Debug`, `release` →
    /// `Release`). Passed through to the tool, which validates it.
    pub build_type: String,
    /// Parallel jobs (`--parallel N` / `-j N`). `None` = tool default.
    /// (The meson adapter currently leaves this to the backend.)
    pub jobs: Option<usize>,
    /// Detected rivet toolchain, when known. Its `vcvars` env overlay is
    /// applied to every adapter subprocess (so MSVC builds work outside a
    /// Developer Prompt) and its C++ driver is forwarded as
    /// `CMAKE_CXX_COMPILER` unless the spec already sets it. `None` = rely
    /// on the tool's own compiler discovery.
    pub toolchain: Option<CxxToolchain>,
}

impl AdapterCtx {
    /// `Debug` build in `build_dir`, tool-default jobs, no toolchain env.
    pub fn new(build_dir: PathBuf) -> Self {
        Self {
            build_dir,
            build_type: "Debug".to_string(),
            jobs: None,
            toolchain: None,
        }
    }

    /// Override the build type (`Debug`/`Release`).
    pub fn with_build_type(mut self, build_type: impl Into<String>) -> Self {
        self.build_type = build_type.into();
        self
    }

    /// Override parallel jobs.
    pub fn with_jobs(mut self, jobs: usize) -> Self {
        self.jobs = Some(jobs);
        self
    }

    /// Attach the detected toolchain (MSVC env + compiler forwarding).
    pub fn with_toolchain(mut self, toolchain: CxxToolchain) -> Self {
        self.toolchain = Some(toolchain);
        self
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Validate an adapter spec: known tool + non-empty option keys.
pub fn validate_spec(spec: &AdapterSpec) -> Result<AdapterKind, Error> {
    let kind = AdapterKind::from_tool(&spec.tool)?;
    for key in spec.options.keys() {
        if key.trim().is_empty() {
            return Err(Error::InvalidSpec {
                message: "adapter `options` has an empty key. Fix: remove it or spell the CMake definition name".to_string(),
            });
        }
    }
    Ok(kind)
}

/// Build a foreign source dir with the tool named by `spec`.
///
/// Returns include dirs + produced libs for the integrator. Fails
/// actionably when the tool is missing, the source dir is unusable, the
/// configure/build step fails, or `targets` match nothing built.
/// `pkg-config`/`system` tools take no sources — use [`probe_installed`].
pub fn build_adapter(
    source_dir: &Path,
    spec: &AdapterSpec,
    ctx: &AdapterCtx,
) -> Result<AdapterOutput, Error> {
    if !source_dir.is_dir() {
        return Err(Error::InvalidSpec {
            message: format!(
                "adapter source directory `{}` does not exist or is not a directory. Fix: check the dependency `path`/checkout subdir",
                source_dir.display()
            ),
        });
    }
    match validate_spec(spec)? {
        AdapterKind::Cmake => cmake::build(source_dir, spec, ctx),
        AdapterKind::Meson => meson::build(source_dir, spec, ctx),
        AdapterKind::Make => make::build(source_dir, spec, ctx),
        AdapterKind::PkgConfig | AdapterKind::System => Err(Error::InvalidSpec {
            message: format!(
                "adapter tool `{}` probes an installed library and takes no sources. Fix: use `probe_installed(\"{}\", <package>)` instead of `build_adapter`",
                spec.tool, spec.tool
            ),
        }),
    }
}

/// Probe an installed library by package name (sourceless deps).
///
/// `tool` is `pkg-config` (strict) or `system` (pkg-config, then a
/// well-known-dir search). Source-building tools are rejected with an
/// actionable error pointing at [`build_adapter`].
pub fn probe_installed(tool: &str, package: &str) -> Result<AdapterOutput, Error> {
    match AdapterKind::from_tool(tool)? {
        AdapterKind::PkgConfig => probe_pkg_config(package),
        AdapterKind::System => probe_system(package),
        kind => Err(Error::InvalidSpec {
            message: format!(
                "adapter tool `{}` builds from sources and needs a source dir. Fix: use `build_adapter(<source>, spec, ctx)` instead of `probe_installed`",
                kind.as_str()
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn spec(tool: &str) -> AdapterSpec {
        AdapterSpec {
            tool: tool.to_string(),
            targets: Vec::new(),
            options: BTreeMap::new(),
        }
    }

    #[test]
    fn kinds_from_tool() {
        assert_eq!(AdapterKind::from_tool("cmake").unwrap(), AdapterKind::Cmake);
        assert_eq!(AdapterKind::from_tool("meson").unwrap(), AdapterKind::Meson);
        assert_eq!(AdapterKind::from_tool("make").unwrap(), AdapterKind::Make);
        assert_eq!(
            AdapterKind::from_tool("pkg-config").unwrap(),
            AdapterKind::PkgConfig
        );
        assert_eq!(
            AdapterKind::from_tool("system").unwrap(),
            AdapterKind::System
        );
        assert!(AdapterKind::Cmake.builds_sources());
        assert!(!AdapterKind::System.builds_sources());
    }

    #[test]
    fn unknown_and_empty_tools_rejected() {
        let err = AdapterKind::from_tool("ninja").unwrap_err().to_string();
        assert!(err.contains("ninja"), "got: {err}");
        assert!(err.contains("cmake"), "got: {err}");
        let err = AdapterKind::from_tool("").unwrap_err().to_string();
        assert!(err.contains("adapter.tool"), "got: {err}");
    }

    #[test]
    fn empty_option_key_rejected() {
        let mut s = spec("cmake");
        s.options.insert(String::new(), "1".to_string());
        let err = validate_spec(&s).unwrap_err().to_string();
        assert!(err.contains("empty key"), "got: {err}");
    }

    #[test]
    fn dispatch_points_at_the_right_entry_point() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = AdapterCtx::new(dir.path().join("build"));
        // Sourceless tools via build_adapter.
        let err = build_adapter(dir.path(), &spec("system"), &ctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("probe_installed"), "got: {err}");
        let err = build_adapter(dir.path(), &spec("pkg-config"), &ctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("probe_installed"), "got: {err}");
        // Source tools via probe_installed.
        let err = probe_installed("cmake", "mini").unwrap_err().to_string();
        assert!(err.contains("build_adapter"), "got: {err}");
        let err = probe_installed("ninja", "mini").unwrap_err().to_string();
        assert!(err.contains("ninja"), "got: {err}");
    }

    #[test]
    fn missing_source_dir_is_actionable() {
        let ctx = AdapterCtx::new(PathBuf::from("build"));
        let err = build_adapter(Path::new("does-not-exist"), &spec("cmake"), &ctx)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does-not-exist"), "got: {err}");
    }

    #[test]
    fn empty_package_name_rejected() {
        let err = probe_pkg_config("").unwrap_err().to_string();
        assert!(err.contains("empty"), "got: {err}");
        let err = probe_system("").unwrap_err().to_string();
        assert!(err.contains("empty"), "got: {err}");
    }
}
