//! C++ toolchain detection and flag normalization.
//!
//! Detects MSVC (via `vswhere` / well-known VS install paths / `vcvars`
//! environment capture), `clang-cl`, `clang`, `gcc`, and MinGW. All compiler
//! invocations use `Path`/`PathBuf` + `OsString`; no global mutable state.
//!
//! Detection order: `cl` in `PATH` → `vswhere` → well-known VS paths →
//! `clang-cl` → `clang++` → `g++`. The first usable toolchain wins; every
//! candidate tried is recorded so `-v` can log the decision and failures can
//! say exactly what was tried and how to fix it.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

pub mod target;

pub use target::{select_for_target, TargetSelection, TargetTriple};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Toolchain errors. Always actionable: what was tried + how to fix.
#[derive(Debug, Error)]
pub enum Error {
    /// No usable C++ compiler found.
    #[error("no C++ toolchain found.\nTried:\n{tried}\nFix: install one of:\n  - Windows: Visual Studio Build Tools (https://visualstudio.microsoft.com/downloads, check \"Desktop development with C++\"), or `winget install LLVM.LLVM`\n  - Linux: `sudo apt install build-essential clang` (or `dnf groupinstall \"Development Tools\"`)")]
    NoToolchain {
        /// Human-readable list of candidates tried.
        tried: String,
    },
    /// Compiler invocation failed.
    #[error("{what}: {message}")]
    CommandFailed {
        /// What was attempted.
        what: String,
        /// Detail + fix hint.
        message: String,
    },
    /// IO failure.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// A sanitizer is not supported by this toolchain.
    #[error("sanitizer `{name}` is not supported by the {kind} toolchain. Fix: use `--features`/profile `sanitizers` with only {supported}")]
    UnsupportedSanitizer {
        /// Sanitizer name as written.
        name: String,
        /// Toolchain kind string.
        kind: String,
        /// Supported sanitizer list for the message.
        supported: String,
    },
    /// A `--target` triple is malformed or names an unsupported arch/OS/env.
    #[error("invalid target triple `{input}`: {reason}. Fix: use `native` or `arch-vendor-os[-env]`, e.g. `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`")]
    InvalidTarget {
        /// Triple as written.
        input: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A cross-compile target has no usable compiler installed.
    #[error("cannot target `{requested}` from `{host}`: no cross-capable compiler found.\nTried:\n  - {tried}\nFix: install LLVM clang (`winget install LLVM.LLVM` on Windows, `sudo apt install clang` on Linux) so rivet can pass `--target={requested}` to clang")]
    NoCrossToolchain {
        /// Requested triple.
        requested: String,
        /// Host triple.
        host: String,
        /// Candidates tried.
        tried: String,
    },
    /// Precompiled headers need per-TU ordering the build graph has no
    /// model for yet (M7 verdict: actionable error, not silent fallback).
    #[error("precompiled headers are not supported yet ({reason}). Fix: keep headers in `include/` and `#include` them normally")]
    PchNotSupported {
        /// Why PCH cannot be expressed in the flag model.
        reason: String,
    },
    /// C++20 modules need per-toolchain scanning the build graph has no
    /// model for yet (M7 verdict: actionable error, not silent fallback).
    #[error(
        "C++20 modules are not supported yet ({reason}). Fix: keep using header-based includes"
    )]
    CxxModulesNotSupported {
        /// Why modules cannot be expressed in the flag model.
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

/// Compiler family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolchainKind {
    /// Microsoft Visual C++ (`cl.exe`).
    Msvc,
    /// LLVM clang in MSVC-compat mode (`clang-cl.exe`).
    ClangCl,
    /// LLVM clang (`clang++`).
    Clang,
    /// GNU compiler (`g++`).
    Gcc,
    /// MinGW-w64 gcc on Windows.
    Mingw,
}

impl std::fmt::Display for ToolchainKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Msvc => write!(f, "msvc"),
            Self::ClangCl => write!(f, "clang-cl"),
            Self::Clang => write!(f, "clang"),
            Self::Gcc => write!(f, "gcc"),
            Self::Mingw => write!(f, "mingw"),
        }
    }
}

/// A detected C++ toolchain.
#[derive(Debug, Clone)]
pub struct CxxToolchain {
    /// Compiler family.
    pub kind: ToolchainKind,
    /// Path to the C++ driver (`cl.exe`, `clang++`, `g++`).
    pub cxx: PathBuf,
    /// Detected version string (first line of `--version` / `cl` banner).
    pub version: String,
    /// MSVC environment overlay captured from `vcvars` (`INCLUDE`, `LIB`,
    /// `LIBPATH`, `PATH`, SDK vars). `None` for non-MSVC toolchains or when
    /// the inherited environment already works (dev prompt).
    pub msvc_env: Option<Vec<(OsString, OsString)>>,
    /// Path to the static archiver (`lib.exe`, `ar`, `llvm-lib`), when one
    /// was found. `None` means archiving fails with an actionable error.
    pub ar: Option<PathBuf>,
    /// Requested `--target` triple. `None` = native detection (fingerprint
    /// byte-identical to earlier releases). Set by the coordinator from
    /// `TargetSelection`; cross args derive from it via [`Self::target_args`].
    pub target: Option<TargetTriple>,
    /// How this toolchain was found (for `-v` logging).
    pub found_via: String,
}

/// How header dependencies are tracked for a toolchain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepStyle {
    /// `-MMD -MF <depfile>`: the compiler writes a Makefile-style depfile.
    GccDepfile,
    /// `/showIncludes`: includes are parsed from captured stdout.
    MsvcShowIncludes,
}

impl CxxToolchain {
    /// Assemble a detected toolchain, probing for its static archiver.
    fn with_archiver(
        kind: ToolchainKind,
        cxx: PathBuf,
        version: String,
        msvc_env: Option<Vec<(OsString, OsString)>>,
        found_via: String,
    ) -> Self {
        let ar = detect_archiver(kind, &cxx);
        Self {
            kind,
            cxx,
            version,
            msvc_env,
            ar,
            target: None,
            found_via,
        }
    }

    /// Extra driver args for the selected target (empty for native).
    ///
    /// Cross builds compile with `clang++ --target=<triple>`; the flag is
    /// appended to compile and link commands (never to `ar`, which stays
    /// native — see `select_for_target`).
    pub fn target_args(&self) -> Vec<OsString> {
        match &self.target {
            Some(t) if !t.is_native() => {
                vec![OsString::from(format!("--target={}", t.raw))]
            }
            _ => Vec::new(),
        }
    }
    /// `true` for `cl.exe`-style drivers (MSVC and clang-cl).
    pub fn is_msvc_like(&self) -> bool {
        matches!(self.kind, ToolchainKind::Msvc | ToolchainKind::ClangCl)
    }

    /// Executable suffix for built binaries on this host.
    pub fn exe_suffix(&self) -> &'static str {
        if cfg!(windows) {
            ".exe"
        } else {
            ""
        }
    }

    /// Content fingerprint: ABI/compiler/std/stdlib inputs that must
    /// invalidate the build cache. Covers kind, version, std, opt, debug,
    /// defines, include dirs (in order — order is semantics for `-I`), lto,
    /// sanitizers, enabled features (sorted), and the `--target` selection
    /// (only when set — native `None` keeps the historic format byte for
    /// byte, so native builds never rebuild spuriously).
    ///
    /// Format (M3, also consumed by `rivet-graph` link digests; M4 lockfile
    /// readers must treat it as opaque but stable within a toolchain):
    /// `<kind>|<version>|std=<std>|opt=<n>|debug=<0|1>|defines=<;>|includes=<;>`
    /// `|lto=<0|1>|sanitizers=<csv>|features=<csv>[|target=<triple>]`.
    pub fn fingerprint(&self, opts: &CompileOptions) -> String {
        let defines = opts.defines.join(";");
        let includes = opts
            .include_dirs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(";");
        let base = format!(
            "{}|{}|std={}|opt={}|debug={}|defines={defines}|includes={includes}|lto={}|sanitizers={}|features={}",
            self.kind,
            self.version.trim(),
            opts.std,
            opts.opt,
            opts.debug as u8,
            opts.lto as u8,
            opts.sanitizers.join(","),
            opts.features.join(","),
        );
        match &self.target {
            Some(t) => format!("{base}|target={}", t.raw),
            None => base,
        }
    }

    /// Fingerprint plus the `--target` selection (M7).
    ///
    /// This is `fingerprint()` with [`TargetSelection::target_suffix`]
    /// appended (`|target=native` by default, `|target=<triple>` when
    /// `--target` names one). `fingerprint()` itself is byte-identical to
    /// M3 so native builds never rebuild spuriously; the coordinator uses
    /// this variant so a target switch rebuilds exactly once.
    pub fn fingerprint_with_target(
        &self,
        opts: &CompileOptions,
        target: Option<&TargetTriple>,
    ) -> String {
        let base = self.fingerprint(opts);
        match target {
            None => format!("{base}|target=native"),
            Some(t) => format!("{base}|target={}", t.raw),
        }
    }

    /// Precompiled-header *create* flags (M7 verdict: unsupported).
    ///
    /// MSVC needs `/Yc` on exactly one TU built before all `/Yu` consumers
    /// (plus a shared `/Fp` file); GCC/Clang need the `.gch` built first
    /// with identical flags and a forced `-include`. The flag model
    /// compiles every TU uniformly with no ordering, so no flag list here
    /// could be correct — this returns an actionable error instead of
    /// silently emitting half a PCH setup. The graph needs a PCH-aware
    /// planning step first (future work, not M7).
    pub fn pch_create_flags(
        &self,
        _header: &Path,
        _output: &Path,
        _opts: &CompileOptions,
    ) -> Result<Vec<OsString>, Error> {
        Err(Error::PchNotSupported {
            reason: "creating a PCH must precede (and share flags with) every consuming TU, but the build graph compiles all TUs uniformly with no ordering".to_string(),
        })
    }

    /// Precompiled-header *use* flags (M7 verdict: unsupported — see
    /// [`CxxToolchain::pch_create_flags`]).
    pub fn pch_use_flags(
        &self,
        _header: &Path,
        _opts: &CompileOptions,
    ) -> Result<Vec<OsString>, Error> {
        Err(Error::PchNotSupported {
            reason: "consuming a PCH (`/Yu` + `/Fp` on MSVC, `-include` + `.gch` on GCC/Clang) requires the PCH artifact to exist first, which the build graph cannot order yet".to_string(),
        })
    }

    /// C++20 module flags (M7 verdict: unsupported).
    ///
    /// Module compilation needs a compiler-specific dependency scan first
    /// (MSVC `/scanDependencies`, Clang `-fmodule-mapper`/`--precompile`
    /// DAG, GCC `-fmodules-ts` BMI ordering) to discover the build DAG
    /// before compiling anything. Flags alone cannot express that; the
    /// graph needs module-aware planning first (future work, not M7).
    pub fn cxx_module_flags(&self, _opts: &CompileOptions) -> Result<Vec<OsString>, Error> {
        Err(Error::CxxModulesNotSupported {
            reason: "compilers disagree on module scanning (`/scanDependencies` vs BMI DAGs) and the build graph has no module-aware planning step yet".to_string(),
        })
    }

    /// Reject sanitizer names this toolchain cannot honor.
    ///
    /// MSVC supports only `address`; anything else is an actionable error
    /// (never silently dropped). GNU/Clang names pass through — the compiler
    /// itself reports unknown names. Callers must run this before generating
    /// commands, since command builders are infallible.
    pub fn validate_options(&self, opts: &CompileOptions) -> Result<(), Error> {
        if self.is_msvc_like() {
            for name in &opts.sanitizers {
                if name != "address" {
                    return Err(Error::UnsupportedSanitizer {
                        name: name.clone(),
                        kind: self.kind.to_string(),
                        supported: "`address` (the only sanitizer MSVC supports)".to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Header-tracking style for this toolchain.
    pub fn dep_style(&self) -> DepStyle {
        if self.is_msvc_like() {
            DepStyle::MsvcShowIncludes
        } else {
            DepStyle::GccDepfile
        }
    }

    /// Object file extension without the dot (`obj` or `o`).
    pub fn obj_extension(&self) -> &'static str {
        if self.is_msvc_like() {
            "obj"
        } else {
            "o"
        }
    }

    /// Static archive file name for a library (`hello.lib` / `libhello.a`).
    pub fn staticlib_filename(&self, name: &str) -> String {
        if self.is_msvc_like() {
            format!("{name}.lib")
        } else {
            format!("lib{name}.a")
        }
    }

    /// Shared flag core for a compile-only step (no dep tracking, no env).
    ///
    /// LTO/sanitizer mapping (M3 profiles): MSVC-like `/GL` + GNU/Clang
    /// `-flto`; sanitizers as `/fsanitize=<csv>` (MSVC-like, validated to
    /// `address`-only by [`CxxToolchain::validate_options`]) or
    /// `-fsanitize=<csv>` (GNU/Clang). `clang-cl` follows the MSVC mapping.
    fn push_common_args(&self, args: &mut Vec<OsString>, opts: &CompileOptions) {
        if self.is_msvc_like() {
            args.push(OsString::from("/nologo"));
            args.push(OsString::from("/EHsc"));
            args.push(OsString::from(format!("/std:{}", opts.std)));
            match opts.opt {
                0 => args.push(OsString::from("/Od")),
                1 => args.push(OsString::from("/O1")),
                _ => args.push(OsString::from("/O2")),
            }
            if opts.debug {
                args.push(OsString::from("/Zi"));
            }
            if opts.lto {
                args.push(OsString::from("/GL"));
            }
            if !opts.sanitizers.is_empty() {
                args.push(OsString::from(format!(
                    "/fsanitize={}",
                    opts.sanitizers.join(",")
                )));
            }
            for d in &opts.defines {
                args.push(OsString::from(format!("/D{d}")));
            }
            for inc in &opts.include_dirs {
                let mut s = OsString::from("/I");
                s.push(inc);
                args.push(s);
            }
        } else {
            args.push(OsString::from(format!("-std={}", opts.std)));
            args.push(OsString::from(format!("-O{}", opts.opt.min(3))));
            if opts.debug {
                args.push(OsString::from("-g"));
            }
            if opts.lto {
                args.push(OsString::from("-flto"));
            }
            if !opts.sanitizers.is_empty() {
                args.push(OsString::from(format!(
                    "-fsanitize={}",
                    opts.sanitizers.join(",")
                )));
            }
            for d in &opts.defines {
                args.push(OsString::from(format!("-D{d}")));
            }
            for inc in &opts.include_dirs {
                let mut s = OsString::from("-I");
                s.push(inc);
                args.push(s);
            }
        }
    }

    /// Flags for a compile-only step (`source` → `object`).
    pub fn compile_only_args(
        &self,
        source: &Path,
        object: &Path,
        opts: &CompileOptions,
    ) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        self.push_common_args(&mut args, opts);
        if self.is_msvc_like() {
            args.push(OsString::from("/c"));
            args.push(source.as_os_str().to_os_string());
            let mut fo = OsString::from("/Fo");
            fo.push(object);
            args.push(fo);
        } else {
            args.push(OsString::from("-c"));
            args.push(source.as_os_str().to_os_string());
            args.push(OsString::from("-o"));
            args.push(object.as_os_str().to_os_string());
        }
        args
    }

    /// Build a per-TU compile `Command` with header tracking enabled.
    ///
    /// - GCC/Clang: `depfile` must be `Some` (the caller creates its parent);
    ///   adds `-MMD -MF <depfile> -MT <object>`.
    /// - MSVC-like: always adds `/showIncludes` (parsed from captured stdout)
    ///   plus `/Fd` next to the object so parallel `cl` processes never share
    ///   the default `vc*.pdb`.
    ///
    /// The command carries the `vcvars` overlay and `VSLANG=1033` (English
    /// `/showIncludes` notes) where applicable. Callers must capture (not
    /// inherit) stdout for MSVC-like toolchains.
    pub fn compile_command(
        &self,
        source: &Path,
        object: &Path,
        depfile: Option<&Path>,
        opts: &CompileOptions,
    ) -> Command {
        let mut cmd = Command::new(&self.cxx);
        let mut args: Vec<OsString> = Vec::new();
        self.push_common_args(&mut args, opts);
        if self.is_msvc_like() {
            args.push(OsString::from("/showIncludes"));
            args.push(OsString::from("/c"));
            args.push(source.as_os_str().to_os_string());
            let mut fo = OsString::from("/Fo");
            fo.push(object);
            args.push(fo);
            if opts.debug {
                let mut fd = OsString::from("/Fd");
                fd.push(object.with_extension("pdb"));
                args.push(fd);
            }
        } else {
            if let Some(dep) = depfile {
                args.push(OsString::from("-MMD"));
                args.push(OsString::from("-MF"));
                args.push(dep.as_os_str().to_os_string());
                args.push(OsString::from("-MT"));
                // Forward slashes keep the `-MT` label (and depfile) free of
                // backslash-escape ambiguity on Windows.
                args.push(OsString::from(
                    object.display().to_string().replace('\\', "/"),
                ));
            }
            args.push(OsString::from("-c"));
            args.push(source.as_os_str().to_os_string());
            args.push(OsString::from("-o"));
            args.push(object.as_os_str().to_os_string());
        }
        args.extend(self.target_args());
        cmd.args(&args);
        self.apply_env(&mut cmd);
        cmd
    }

    /// Build a link `Command` for `objects` + static `archives` → `output`.
    ///
    /// The `cl` driver doubles as the link driver on MSVC-like toolchains
    /// (no separate `link.exe` lookup). Archives are absolute paths, so no
    /// `/LIBPATH`/`-L` search dirs are needed. LTO/sanitizers ride along so
    /// link-time codegen and runtimes match the compile step: MSVC-like
    /// `/fsanitize=<csv>` (cl-level) + `/LTCG` (link-level), GNU/Clang
    /// `-flto` / `-fsanitize=<csv>`.
    /// `lib_dirs` join the search path (`-L` / `/LIBPATH`) and `sys_libs`
    /// join as bare names (`-l<name>` / `<name>.lib`) — fed by adapter
    /// probes, system libs, and `rivet:lib -lname` script directives.
    pub fn link_exe_command(
        &self,
        objects: &[PathBuf],
        archives: &[PathBuf],
        lib_dirs: &[PathBuf],
        sys_libs: &[String],
        output: &Path,
        opts: &CompileOptions,
    ) -> Command {
        let mut cmd = Command::new(&self.cxx);
        if self.is_msvc_like() {
            cmd.arg("/nologo");
            for o in objects {
                cmd.arg(o);
            }
            for a in archives {
                cmd.arg(a);
            }
            if !opts.sanitizers.is_empty() {
                cmd.arg(format!("/fsanitize={}", opts.sanitizers.join(",")));
            }
            let mut fe = OsString::from("/Fe");
            fe.push(output);
            cmd.arg(fe);
            let mut link_opts: Vec<&str> = Vec::new();
            if opts.debug {
                link_opts.push("/DEBUG");
            }
            if opts.lto {
                link_opts.push("/LTCG");
            }
            // Linker options (search paths, bare libs) go after `/link`;
            // `cl` rejects them as source files otherwise (D9002/D9024).
            if !link_opts.is_empty() || !lib_dirs.is_empty() || !sys_libs.is_empty() {
                cmd.arg("/link");
            }
            for o in link_opts {
                cmd.arg(o);
            }
            for dir in lib_dirs {
                let mut lp = OsString::from("/LIBPATH:");
                lp.push(dir);
                cmd.arg(lp);
            }
            for name in sys_libs {
                cmd.arg(msvc_lib_name(name));
            }
        } else {
            for o in objects {
                cmd.arg(o);
            }
            for a in archives {
                cmd.arg(a);
            }
            for dir in lib_dirs {
                let mut l = OsString::from("-L");
                l.push(dir);
                cmd.arg(l);
            }
            for name in sys_libs {
                cmd.arg(format!("-l{name}"));
            }
            if opts.lto {
                cmd.arg("-flto");
            }
            if !opts.sanitizers.is_empty() {
                cmd.arg(format!("-fsanitize={}", opts.sanitizers.join(",")));
            }
            cmd.arg("-o");
            cmd.arg(output);
        }
        cmd.args(self.target_args());
        self.apply_env(&mut cmd);
        cmd
    }

    /// Build an archive `Command` for `objects` → static library `output`.
    pub fn archive_command(&self, objects: &[PathBuf], output: &Path) -> Result<Command, Error> {
        let ar = self.ar.clone().ok_or_else(|| Error::CommandFailed {
            what: "archive static library".to_string(),
            message: format!(
                "no archiver found for {} ({}). Fix: install the full toolchain (MSVC Build Tools `lib.exe`, or binutils `ar`)",
                self.kind,
                self.cxx.display()
            ),
        })?;
        let mut cmd = Command::new(&ar);
        if self.is_msvc_like() {
            cmd.arg("/NOLOGO");
            let mut out = OsString::from("/OUT:");
            out.push(output);
            cmd.arg(out);
            for o in objects {
                cmd.arg(o);
            }
        } else {
            cmd.arg("crus");
            cmd.arg(output);
            for o in objects {
                cmd.arg(o);
            }
        }
        self.apply_env(&mut cmd);
        Ok(cmd)
    }

    /// Apply the captured `vcvars` environment overlay, plus `VSLANG=1033`
    /// on MSVC-like toolchains so `/showIncludes` notes are always the
    /// English `Note: including file:` form regardless of VS locale (D16).
    pub fn apply_env(&self, cmd: &mut Command) {
        if let Some(env) = &self.msvc_env {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }
        if self.is_msvc_like() {
            cmd.env("VSLANG", "1033");
        }
    }
}

/// MSVC link name for a bare library: `foo` → `foo.lib` (already-suffixed
/// names pass through).
fn msvc_lib_name(name: &str) -> String {
    if name.contains('.') {
        name.to_string()
    } else {
        format!("{name}.lib")
    }
}

/// Normalized per-build compile options. The toolchain maps these to
/// compiler-specific flags, so the rest of rivet never branches on `cl`
/// vs `g++` directly.
#[derive(Debug, Clone)]
pub struct CompileOptions {
    /// C++ standard, e.g. `c++20`.
    pub std: String,
    /// Optimization level 0–3.
    pub opt: u8,
    /// Emit debug info.
    pub debug: bool,
    /// Preprocessor defines (`NAME` or `NAME=value`).
    pub defines: Vec<String>,
    /// Extra include directories.
    pub include_dirs: Vec<PathBuf>,
    /// Link-time optimization (M3 profiles).
    pub lto: bool,
    /// Sanitizers, e.g. `["address"]` (M3 profiles; MSVC supports only
    /// `address` — see [`CxxToolchain::validate_options`]).
    pub sanitizers: Vec<String>,
    /// Enabled features, sorted (M3 contract C1; fingerprint only — no flags
    /// are derived from features).
    pub features: Vec<String>,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            std: "c++17".to_string(),
            opt: 0,
            debug: true,
            defines: Vec::new(),
            include_dirs: Vec::new(),
            lto: false,
            sanitizers: Vec::new(),
            features: Vec::new(),
        }
    }
}

/// One entry for `rivet toolchains`.
#[derive(Debug, Clone)]
pub struct ToolchainInfo {
    /// Compiler family.
    pub kind: ToolchainKind,
    /// Path to the driver.
    pub path: PathBuf,
    /// Version string or failure reason.
    pub version: String,
    /// Whether this entry is the auto-selected default.
    pub is_default: bool,
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// Result of verbose detection: the toolchain plus the decision log.
#[derive(Debug)]
pub struct Detection {
    /// Selected toolchain.
    pub toolchain: CxxToolchain,
    /// One line per candidate tried (for `-v`).
    pub log: Vec<String>,
}

/// Detect the default toolchain, returning the decision log too.
pub fn detect_verbose() -> Result<Detection, Error> {
    let mut log: Vec<String> = Vec::new();
    let mut tried: Vec<String> = Vec::new();

    // 1. cl.exe in PATH (dev prompt or pre-configured env).
    match which::which("cl") {
        Ok(p) => {
            log.push(format!("found cl in PATH: {}", p.display()));
            match probe_msvc_inherited_env(&p) {
                Ok(tc) => return Ok(Detection { toolchain: tc, log }),
                Err(e) => {
                    log.push(format!("cl in PATH unusable: {e}"));
                    tried.push(format!("cl in PATH ({}): {e}", p.display()));
                }
            }
        }
        Err(_) => {
            log.push("cl not in PATH".to_string());
            tried.push("cl in PATH: not found".to_string());
        }
    }

    // 2–3. vswhere + well-known VS paths (Windows only).
    #[cfg(windows)]
    {
        match find_msvc_via_vswhere(&mut log) {
            Some(Ok(tc)) => return Ok(Detection { toolchain: tc, log }),
            Some(Err(e)) => tried.push(format!("MSVC via vswhere: {e}")),
            None => {
                tried.push("MSVC via vswhere: vswhere not found or no VS with VC.Tools".to_string())
            }
        }
        match find_msvc_via_known_paths(&mut log) {
            Some(tc) => return Ok(Detection { toolchain: tc, log }),
            None => tried.push(
                "MSVC via well-known VS paths: no cl.exe under Visual Studio/BuildTools"
                    .to_string(),
            ),
        }
    }

    // 4. clang-cl in PATH.
    match which::which("clang-cl") {
        Ok(p) => {
            log.push(format!("found clang-cl in PATH: {}", p.display()));
            let version = query_version(&p, &[]);
            let tc = CxxToolchain::with_archiver(
                ToolchainKind::ClangCl,
                p,
                version,
                None,
                "clang-cl in PATH".to_string(),
            );
            return Ok(Detection { toolchain: tc, log });
        }
        Err(_) => {
            log.push("clang-cl not in PATH".to_string());
            tried.push("clang-cl in PATH: not found".to_string());
        }
    }

    // 5. clang++ in PATH.
    match which::which("clang++") {
        Ok(p) => {
            log.push(format!("found clang++ in PATH: {}", p.display()));
            let version = query_version(&p, &[OsString::from("--version")]);
            let tc = CxxToolchain::with_archiver(
                ToolchainKind::Clang,
                p,
                version,
                None,
                "clang++ in PATH".to_string(),
            );
            return Ok(Detection { toolchain: tc, log });
        }
        Err(_) => {
            log.push("clang++ not in PATH".to_string());
            tried.push("clang++ in PATH: not found".to_string());
        }
    }

    // 6. g++ in PATH (gcc on Linux, MinGW g++ on Windows).
    match which::which("g++") {
        Ok(p) => {
            log.push(format!("found g++ in PATH: {}", p.display()));
            let kind = if cfg!(windows) {
                ToolchainKind::Mingw
            } else {
                ToolchainKind::Gcc
            };
            let version = query_version(&p, &[OsString::from("--version")]);
            let tc = CxxToolchain::with_archiver(kind, p, version, None, "g++ in PATH".to_string());
            return Ok(Detection { toolchain: tc, log });
        }
        Err(_) => {
            log.push("g++ not in PATH".to_string());
            tried.push("g++ in PATH: not found".to_string());
        }
    }

    // 7. c++/cc fallback (Unix).
    #[cfg(not(windows))]
    {
        for name in ["c++", "cc"] {
            match which::which(name) {
                Ok(p) => {
                    log.push(format!("found {name} in PATH: {}", p.display()));
                    let version = query_version(&p, &[OsString::from("--version")]);
                    let tc = CxxToolchain::with_archiver(
                        ToolchainKind::Gcc,
                        p,
                        version,
                        None,
                        format!("{name} in PATH"),
                    );
                    return Ok(Detection { toolchain: tc, log });
                }
                Err(_) => {
                    log.push(format!("{name} not in PATH"));
                    tried.push(format!("{name} in PATH: not found"));
                }
            }
        }
    }

    Err(Error::NoToolchain {
        tried: tried
            .iter()
            .map(|t| format!("  - {t}"))
            .collect::<Vec<_>>()
            .join("\n"),
    })
}

/// Detect the default toolchain.
pub fn detect() -> Result<CxxToolchain, Error> {
    detect_verbose().map(|d| d.toolchain)
}

/// Probe every known toolchain for `rivet toolchains`.
pub fn list_toolchains() -> Vec<ToolchainInfo> {
    let mut out: Vec<ToolchainInfo> = Vec::new();
    let default_cxx = detect().map(|t| t.cxx).unwrap_or_default();

    #[cfg(windows)]
    {
        for cl in scan_known_cl_exes() {
            if !out.iter().any(|i| i.path == cl) {
                out.push(ToolchainInfo {
                    kind: ToolchainKind::Msvc,
                    version: query_version(&cl, &[]),
                    path: cl.clone(),
                    is_default: cl == default_cxx,
                });
            }
        }
        if let Ok(p) = which::which("cl") {
            if !out.iter().any(|i| i.path == p) {
                out.push(ToolchainInfo {
                    kind: ToolchainKind::Msvc,
                    version: query_version(&p, &[]),
                    path: p.clone(),
                    is_default: p == default_cxx,
                });
            }
        }
    }

    for (name, kind) in [
        ("clang-cl", ToolchainKind::ClangCl),
        ("clang++", ToolchainKind::Clang),
        (
            "g++",
            if cfg!(windows) {
                ToolchainKind::Mingw
            } else {
                ToolchainKind::Gcc
            },
        ),
        (
            "gcc",
            if cfg!(windows) {
                ToolchainKind::Mingw
            } else {
                ToolchainKind::Gcc
            },
        ),
    ] {
        if let Ok(p) = which::which(name) {
            if !out.iter().any(|i| i.path == p) {
                out.push(ToolchainInfo {
                    kind,
                    version: query_version(&p, &[OsString::from("--version")]),
                    path: p.clone(),
                    is_default: p == default_cxx,
                });
            }
        }
    }
    if !out.iter().any(|i| i.is_default) && !out.is_empty() {
        if let Ok(def) = detect() {
            if let Some(first) = out.iter_mut().find(|i| i.kind == def.kind) {
                first.is_default = true;
            }
        }
    }
    out
}

// --- version probing -------------------------------------------------------

/// Run the driver and return the first line of combined output,
/// or `<unreadable version>` if it cannot be executed.
fn query_version(driver: &Path, args: &[OsString]) -> String {
    query_version_with_env(driver, args, None)
}

fn query_version_with_env(
    driver: &Path,
    args: &[OsString],
    env: Option<&[(OsString, OsString)]>,
) -> String {
    let mut cmd = Command::new(driver);
    cmd.args(args);
    if let Some(env) = env {
        for (k, v) in env {
            cmd.env(k, v);
        }
    }
    // `cl` with no args prints its banner (to stderr) and exits non-zero;
    // `--version` drivers print to stdout and exit zero. Either is fine.
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) => return format!("<could not run {}: {e}>", driver.display()),
    };
    let mut combined = out.stdout;
    combined.extend_from_slice(&out.stderr);
    let text = String::from_utf8_lossy(&combined);
    text.lines()
        .next()
        .unwrap_or("<empty version output>")
        .trim()
        .to_string()
}

// --- MSVC helpers (Windows) -------------------------------------------------

/// Accept a `cl.exe` found in `PATH`: the inherited environment is assumed
/// to work (dev prompt). Verifies the binary executes.
fn probe_msvc_inherited_env(cl: &Path) -> Result<CxxToolchain, String> {
    let version = query_version(cl, &[]);
    if version.starts_with("<could not run") {
        return Err(version);
    }
    Ok(CxxToolchain::with_archiver(
        ToolchainKind::Msvc,
        cl.to_path_buf(),
        version,
        None,
        "cl in PATH (inherited env)".to_string(),
    ))
}

/// `vswhere -latest ... -property installationPath` → vcvars → cl.
#[cfg(windows)]
fn find_msvc_via_vswhere(log: &mut Vec<String>) -> Option<Result<CxxToolchain, String>> {
    let vswhere = find_vswhere()?;
    log.push(format!("vswhere: {}", vswhere.display()));
    let out = Command::new(&vswhere)
        .args([
            OsStr::new("-latest"),
            OsStr::new("-products"),
            OsStr::new("*"),
            OsStr::new("-requires"),
            OsStr::new("Microsoft.VisualStudio.Component.VC.Tools"),
            OsStr::new("-property"),
            OsStr::new("installationPath"),
        ])
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => return Some(Err(format!("could not run vswhere ({e})"))),
    };
    let install = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if install.is_empty() {
        log.push("vswhere: no installation with VC.Tools".to_string());
        return None;
    }
    log.push(format!("VS installation: {install}"));
    Some(msvc_from_install_dir(Path::new(&install), "vswhere", log))
}

/// Scan well-known VS install roots for `cl.exe`.
#[cfg(windows)]
fn find_msvc_via_known_paths(log: &mut Vec<String>) -> Option<CxxToolchain> {
    let mut hits = scan_known_cl_exes();
    if hits.is_empty() {
        return None;
    }
    // Prefer newest MSVC tools version (path sort approximates it).
    hits.sort();
    let cl = hits.pop().unwrap();
    log.push(format!("MSVC via known paths: {}", cl.display()));
    match msvc_from_cl_path(&cl, "well-known VS paths", log) {
        Ok(tc) => Some(tc),
        Err(e) => {
            log.push(format!("MSVC candidate unusable: {e}"));
            None
        }
    }
}

#[cfg(windows)]
fn find_vswhere() -> Option<PathBuf> {
    if let Ok(p) = which::which("vswhere") {
        return Some(p);
    }
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(base) = std::env::var_os(var) {
            let p = PathBuf::from(base)
                .join("Microsoft Visual Studio")
                .join("Installer")
                .join("vswhere.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// All `cl.exe` copies under well-known VS roots (Hostx64/x64 only).
#[cfg(windows)]
fn scan_known_cl_exes() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(var) {
            let base = PathBuf::from(base);
            roots.push(base.join("Microsoft Visual Studio"));
        }
    }
    roots.sort();
    roots.dedup();

    let mut hits = Vec::new();
    for root in roots {
        // root/<ver>/<Edition|BuildTools>/VC/Tools/MSVC/<toolsver>/bin/Hostx64/x64/cl.exe
        let Ok(editions) = std::fs::read_dir(&root) else {
            continue;
        };
        for ver_dir in editions.flatten() {
            let Ok(ed_list) = std::fs::read_dir(ver_dir.path()) else {
                continue;
            };
            for ed in ed_list.flatten() {
                let msvc_base = ed.path().join("VC").join("Tools").join("MSVC");
                let Ok(tools) = std::fs::read_dir(&msvc_base) else {
                    continue;
                };
                for tool in tools.flatten() {
                    let cl = tool
                        .path()
                        .join("bin")
                        .join("Hostx64")
                        .join("x64")
                        .join("cl.exe");
                    if cl.is_file() {
                        hits.push(cl);
                    }
                }
            }
        }
    }
    hits.sort();
    hits.dedup();
    hits
}

#[cfg(windows)]
fn msvc_from_install_dir(
    install: &Path,
    via: &str,
    log: &mut Vec<String>,
) -> Result<CxxToolchain, String> {
    let msvc_base = install.join("VC").join("Tools").join("MSVC");
    let mut versions: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&msvc_base) {
        for e in entries.flatten() {
            let cl = e
                .path()
                .join("bin")
                .join("Hostx64")
                .join("x64")
                .join("cl.exe");
            if cl.is_file() {
                versions.push(cl);
            }
        }
    }
    versions.sort();
    let cl = versions
        .pop()
        .ok_or_else(|| format!("no cl.exe under {}", msvc_base.display()))?;
    msvc_from_cl_path(&cl, via, log)
}

#[cfg(windows)]
fn msvc_from_cl_path(cl: &Path, via: &str, log: &mut Vec<String>) -> Result<CxxToolchain, String> {
    // cl at <install>/VC/Tools/MSVC/<ver>/bin/Hostx64/x64/cl.exe;
    // vcvars at <install>/VC/Auxiliary/Build/vcvars64.bat.
    let install = cl
        .ancestors()
        .find(|a| {
            a.join("Auxiliary")
                .join("Build")
                .join("vcvars64.bat")
                .is_file()
        })
        .map(Path::to_path_buf);
    let msvc_env = match install {
        Some(dir) => {
            let vcvars = dir.join("Auxiliary").join("Build").join("vcvars64.bat");
            match capture_vcvars_env(&vcvars) {
                Ok(env) => {
                    log.push(format!("captured vcvars env ({} vars)", env.len()));
                    Some(env)
                }
                Err(e) => {
                    log.push(format!("vcvars capture failed ({e}); using inherited env"));
                    None
                }
            }
        }
        None => {
            log.push("vcvars64.bat not found above cl.exe; using inherited env".to_string());
            None
        }
    };
    Ok(CxxToolchain::with_archiver(
        ToolchainKind::Msvc,
        cl.to_path_buf(),
        query_version_with_env(cl, &[], msvc_env.as_deref()),
        msvc_env,
        format!("MSVC {via}"),
    ))
}

/// Probe for the static archiver belonging to a detected compiler.
///
/// - MSVC: `lib.exe` next to `cl.exe`.
/// - clang-cl: `llvm-lib`, else `lib` in `PATH`.
/// - clang/gcc/MinGW: `ar`, else `llvm-ar` in `PATH`.
///
/// (Cross-prefixed binutils, e.g. `x86_64-linux-gnu-ar`, are M7 work.)
fn detect_archiver(kind: ToolchainKind, cxx: &Path) -> Option<PathBuf> {
    match kind {
        ToolchainKind::Msvc => {
            let lib = cxx.parent()?.join("lib.exe");
            lib.is_file().then_some(lib)
        }
        ToolchainKind::ClangCl => which::which("llvm-lib")
            .or_else(|_| which::which("lib"))
            .ok(),
        ToolchainKind::Clang | ToolchainKind::Gcc | ToolchainKind::Mingw => {
            which::which("ar").or_else(|_| which::which("llvm-ar")).ok()
        }
    }
}

/// Run vcvars and parse the resulting `KEY=VALUE` lines (`set` output).
///
/// `cmd.exe` has its own quote rules that differ from the standard Windows
/// argv quoting `std::process::Command` uses, so passing the vcvars command
/// via [`Command::args`] mangles the quotes around the batch path (paths
/// contain spaces). Instead we pass the command line verbatim with
/// `raw_arg` using the documented `cmd /S /C ""<batch>" <rest>"` idiom:
/// the outer quotes are stripped by `cmd` and the inner quoted batch path
/// then executes correctly.
#[cfg(windows)]
fn capture_vcvars_env(vcvars: &Path) -> Result<Vec<(OsString, OsString)>, String> {
    use std::os::windows::process::CommandExt;
    let inner = format!("\"{}\" >NUL && set", vcvars.display());
    let out = Command::new("cmd")
        .raw_arg("/S")
        .raw_arg("/C")
        .raw_arg(format!("\"{inner}\""))
        .output()
        .map_err(|e| format!("could not run vcvars ({e})"))?;
    if !out.status.success() {
        return Err(format!("vcvars exited with {}", out.status));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut env = Vec::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let ku = k.to_ascii_uppercase();
            let keep = ku == "PATH"
                || ku == "INCLUDE"
                || ku == "LIB"
                || ku == "LIBPATH"
                || ku.starts_with("WINDOWSSDK")
                || ku.starts_with("WINDOWS_SDK")
                || ku.starts_with("VCINSTALL")
                || ku.starts_with("VCTOOLS")
                || ku.starts_with("VSINSTALL")
                || ku.starts_with("UCRT")
                || ku.starts_with("UNIVERSALCRT")
                || ku.starts_with("NETFXSDK");
            if keep {
                env.push((OsString::from(k), OsString::from(v)));
            }
        }
    }
    if env.is_empty() {
        return Err("vcvars produced no usable env vars".to_string());
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(kind: ToolchainKind) -> CxxToolchain {
        CxxToolchain {
            kind,
            cxx: PathBuf::from(if kind == ToolchainKind::Msvc {
                "cl.exe"
            } else {
                "g++"
            }),
            version: "test".to_string(),
            msvc_env: None,
            ar: None,
            target: None,
            found_via: "test".to_string(),
        }
    }

    #[test]
    fn msvc_std_flag() {
        let tc = fake(ToolchainKind::Msvc);
        let opts = CompileOptions {
            std: "c++20".to_string(),
            ..Default::default()
        };
        let args = tc.compile_only_args(Path::new("a.cpp"), Path::new("a.obj"), &opts);
        assert!(args.iter().any(|a| a == "/std:c++20"), "got: {args:?}");
    }

    #[test]
    fn gcc_std_flag() {
        let tc = fake(ToolchainKind::Gcc);
        let opts = CompileOptions {
            std: "c++20".to_string(),
            ..Default::default()
        };
        let args = tc.compile_only_args(Path::new("a.cpp"), Path::new("a.o"), &opts);
        assert!(args.iter().any(|a| a == "-std=c++20"), "got: {args:?}");
    }

    #[test]
    fn opt_levels_map() {
        let msvc = fake(ToolchainKind::Msvc);
        let opts0 = CompileOptions {
            opt: 0,
            ..Default::default()
        };
        let a0 = msvc.compile_only_args(Path::new("a.cpp"), Path::new("a.obj"), &opts0);
        assert!(a0.iter().any(|a| a == "/Od"), "got: {a0:?}");
        let gcc = fake(ToolchainKind::Gcc);
        let opts3 = CompileOptions {
            opt: 3,
            debug: false,
            ..Default::default()
        };
        let a3 = gcc.compile_only_args(Path::new("a.cpp"), Path::new("a.o"), &opts3);
        assert!(a3.iter().any(|a| a == "-O3"), "got: {a3:?}");
        assert!(!a3.iter().any(|a| a == "-g"), "got: {a3:?}");
    }

    #[test]
    fn fingerprint_differs_by_std_and_opt() {
        let tc = fake(ToolchainKind::Gcc);
        let a = tc.fingerprint(&CompileOptions {
            std: "c++17".into(),
            opt: 0,
            ..Default::default()
        });
        let b = tc.fingerprint(&CompileOptions {
            std: "c++20".into(),
            opt: 0,
            ..Default::default()
        });
        let c = tc.fingerprint(&CompileOptions {
            std: "c++17".into(),
            opt: 3,
            ..Default::default()
        });
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn clang_cl_is_msvc_like() {
        assert!(fake(ToolchainKind::ClangCl).is_msvc_like());
        assert!(!fake(ToolchainKind::Gcc).is_msvc_like());
    }

    #[test]
    fn dep_styles_split_by_family() {
        assert_eq!(
            fake(ToolchainKind::Msvc).dep_style(),
            DepStyle::MsvcShowIncludes
        );
        assert_eq!(
            fake(ToolchainKind::ClangCl).dep_style(),
            DepStyle::MsvcShowIncludes
        );
        assert_eq!(fake(ToolchainKind::Gcc).dep_style(), DepStyle::GccDepfile);
        assert_eq!(fake(ToolchainKind::Clang).dep_style(), DepStyle::GccDepfile);
    }

    #[test]
    fn artifact_naming() {
        assert_eq!(fake(ToolchainKind::Msvc).obj_extension(), "obj");
        assert_eq!(fake(ToolchainKind::Gcc).obj_extension(), "o");
        assert_eq!(
            fake(ToolchainKind::Msvc).staticlib_filename("foo"),
            "foo.lib"
        );
        assert_eq!(
            fake(ToolchainKind::Gcc).staticlib_filename("foo"),
            "libfoo.a"
        );
    }

    #[test]
    fn fingerprint_covers_defines_and_includes() {
        let tc = fake(ToolchainKind::Gcc);
        let base = CompileOptions::default();
        let mut with_define = base.clone();
        with_define.defines.push("NDEBUG".to_string());
        let mut with_inc = base.clone();
        with_inc.include_dirs.push(PathBuf::from("/x/inc"));
        assert_ne!(tc.fingerprint(&base), tc.fingerprint(&with_define));
        assert_ne!(tc.fingerprint(&base), tc.fingerprint(&with_inc));
    }

    #[test]
    fn archive_without_archiver_errors_actionably() {
        let tc = fake(ToolchainKind::Gcc);
        let err = tc
            .archive_command(&[PathBuf::from("a.o")], Path::new("liba.a"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no archiver"), "got: {err}");
    }

    #[test]
    fn gnu_archive_invocation() {
        let mut tc = fake(ToolchainKind::Gcc);
        tc.ar = Some(PathBuf::from("ar"));
        let cmd = tc
            .archive_command(&[PathBuf::from("a.o")], Path::new("liba.a"))
            .unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"crus".to_string()), "got: {args:?}");
    }

    fn args_of(cmd: &std::process::Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn lto_maps_per_family_on_compile() {
        let msvc = fake(ToolchainKind::Msvc);
        let opts = CompileOptions {
            lto: true,
            ..Default::default()
        };
        let args = msvc.compile_only_args(Path::new("a.cpp"), Path::new("a.obj"), &opts);
        assert!(args.iter().any(|a| a == "/GL"), "got: {args:?}");

        let gcc = fake(ToolchainKind::Gcc);
        let args = gcc.compile_only_args(Path::new("a.cpp"), Path::new("a.o"), &opts);
        assert!(args.iter().any(|a| a == "-flto"), "got: {args:?}");

        let clang = fake(ToolchainKind::Clang);
        let args = clang.compile_only_args(Path::new("a.cpp"), Path::new("a.o"), &opts);
        assert!(args.iter().any(|a| a == "-flto"), "got: {args:?}");

        // Off by default: neither flag appears.
        let plain = CompileOptions::default();
        let args = gcc.compile_only_args(Path::new("a.cpp"), Path::new("a.o"), &plain);
        assert!(!args.iter().any(|a| a == "-flto"), "got: {args:?}");
    }

    #[test]
    fn lto_maps_per_family_on_link() {
        let msvc = fake(ToolchainKind::Msvc);
        let opts = CompileOptions {
            lto: true,
            debug: false,
            ..Default::default()
        };
        let cmd = msvc.link_exe_command(
            &[PathBuf::from("a.obj")],
            &[],
            &[],
            &[],
            Path::new("a.exe"),
            &opts,
        );
        let args = args_of(&cmd);
        assert!(args.iter().any(|a| a == "/LTCG"), "got: {args:?}");
        assert!(args.iter().any(|a| a == "/link"), "got: {args:?}");

        let gcc = fake(ToolchainKind::Gcc);
        let cmd = gcc.link_exe_command(
            &[PathBuf::from("a.o")],
            &[],
            &[],
            &[],
            Path::new("a"),
            &opts,
        );
        let args = args_of(&cmd);
        assert!(args.iter().any(|a| a == "-flto"), "got: {args:?}");
    }

    #[test]
    fn sanitizers_map_per_family() {
        let opts = CompileOptions {
            sanitizers: vec!["address".to_string()],
            ..Default::default()
        };
        let msvc = fake(ToolchainKind::Msvc);
        msvc.validate_options(&opts).unwrap();
        let args = msvc.compile_only_args(Path::new("a.cpp"), Path::new("a.obj"), &opts);
        assert!(
            args.iter().any(|a| a == "/fsanitize=address"),
            "got: {args:?}"
        );
        let link = msvc.link_exe_command(
            &[PathBuf::from("a.obj")],
            &[],
            &[],
            &[],
            Path::new("a.exe"),
            &opts,
        );
        let link_args = args_of(&link);
        assert!(
            link_args.iter().any(|a| a == "/fsanitize=address"),
            "got: {link_args:?}"
        );

        let gcc = fake(ToolchainKind::Gcc);
        gcc.validate_options(&opts).unwrap();
        let args = gcc.compile_only_args(Path::new("a.cpp"), Path::new("a.o"), &opts);
        assert!(
            args.iter().any(|a| a == "-fsanitize=address"),
            "got: {args:?}"
        );
        let link = gcc.link_exe_command(
            &[PathBuf::from("a.o")],
            &[],
            &[],
            &[],
            Path::new("a"),
            &opts,
        );
        let link_args = args_of(&link);
        assert!(
            link_args.iter().any(|a| a == "-fsanitize=address"),
            "got: {link_args:?}"
        );

        // GNU joins several sanitizers as CSV on both steps.
        let multi = CompileOptions {
            sanitizers: vec!["address".to_string(), "undefined".to_string()],
            ..Default::default()
        };
        let args = gcc.compile_only_args(Path::new("a.cpp"), Path::new("a.o"), &multi);
        assert!(
            args.iter().any(|a| a == "-fsanitize=address,undefined"),
            "got: {args:?}"
        );
    }

    #[test]
    fn sys_libs_map_per_family() {
        let msvc = fake(ToolchainKind::Msvc);
        let link = msvc.link_exe_command(
            &[PathBuf::from("a.obj")],
            &[],
            &[PathBuf::from("C:\\libs")],
            &["ws2_32".to_string(), "dbghelp.lib".to_string()],
            Path::new("a.exe"),
            &CompileOptions::default(),
        );
        let args = args_of(&link);
        assert!(
            args.iter().any(|a| a == "/LIBPATH:C:\\libs"),
            "got: {args:?}"
        );
        assert!(args.iter().any(|a| a == "ws2_32.lib"), "got: {args:?}");
        assert!(args.iter().any(|a| a == "dbghelp.lib"), "got: {args:?}");

        let gcc = fake(ToolchainKind::Gcc);
        let link = gcc.link_exe_command(
            &[PathBuf::from("a.o")],
            &[],
            &[PathBuf::from("/usr/lib")],
            &["z".to_string()],
            Path::new("a"),
            &CompileOptions::default(),
        );
        let args = args_of(&link);
        assert!(args.iter().any(|a| a == "-L/usr/lib"), "got: {args:?}");
        assert!(args.iter().any(|a| a == "-lz"), "got: {args:?}");
    }

    #[test]
    fn target_args_empty_for_native() {
        assert!(fake(ToolchainKind::Gcc).target_args().is_empty());
    }

    #[test]
    fn msvc_rejects_non_address_sanitizers_actionably() {
        let msvc = fake(ToolchainKind::Msvc);
        let clang_cl = fake(ToolchainKind::ClangCl);
        for tc in [&msvc, &clang_cl] {
            let opts = CompileOptions {
                sanitizers: vec!["thread".to_string()],
                ..Default::default()
            };
            let err = tc.validate_options(&opts).unwrap_err().to_string();
            assert!(err.contains("thread"), "got: {err}");
            assert!(err.contains("address"), "got: {err}");
        }
        // GNU/Clang pass validation through (the compiler reports unknowns).
        let gcc = fake(ToolchainKind::Gcc);
        gcc.validate_options(&CompileOptions {
            sanitizers: vec!["thread".to_string()],
            ..Default::default()
        })
        .unwrap();
    }

    #[test]
    fn fingerprint_covers_lto_sanitizers_features() {
        let tc = fake(ToolchainKind::Gcc);
        let base = CompileOptions::default();
        let mut lto = base.clone();
        lto.lto = true;
        let mut asan = base.clone();
        asan.sanitizers.push("address".to_string());
        let mut feat = base.clone();
        feat.features.push("extra".to_string());
        assert_ne!(tc.fingerprint(&base), tc.fingerprint(&lto));
        assert_ne!(tc.fingerprint(&base), tc.fingerprint(&asan));
        assert_ne!(tc.fingerprint(&base), tc.fingerprint(&feat));
    }

    #[test]
    fn fingerprint_with_target_appends_without_changing_base() {
        let tc = fake(ToolchainKind::Gcc);
        let opts = CompileOptions::default();
        let base = tc.fingerprint(&opts);
        // The M3 format is untouched (no spurious rebuilds) …
        assert!(!base.contains("|target="), "got: {base}");
        // … and the target variant only appends.
        assert_eq!(
            tc.fingerprint_with_target(&opts, None),
            format!("{base}|target=native")
        );
        let linux = TargetTriple::parse("x86_64-unknown-linux-gnu").unwrap();
        let windows = TargetTriple::parse("x86_64-pc-windows-msvc").unwrap();
        assert_eq!(
            tc.fingerprint_with_target(&opts, Some(&linux)),
            format!("{base}|target=x86_64-unknown-linux-gnu")
        );
        assert_ne!(
            tc.fingerprint_with_target(&opts, Some(&linux)),
            tc.fingerprint_with_target(&opts, Some(&windows))
        );
    }

    #[test]
    fn pch_and_modules_fail_actionably() {
        let tc = fake(ToolchainKind::Msvc);
        let opts = CompileOptions::default();
        for err in [
            tc.pch_create_flags(Path::new("pch.h"), Path::new("pch.pch"), &opts)
                .unwrap_err()
                .to_string(),
            tc.pch_use_flags(Path::new("pch.h"), &opts)
                .unwrap_err()
                .to_string(),
        ] {
            assert!(err.contains("precompiled headers"), "got: {err}");
            assert!(err.contains("include/"), "got: {err}");
        }
        let err = tc.cxx_module_flags(&opts).unwrap_err().to_string();
        assert!(err.contains("modules"), "got: {err}");
        // GCC agrees: the verdict is toolchain-independent.
        let gcc = fake(ToolchainKind::Gcc);
        assert!(gcc.pch_use_flags(Path::new("pch.h"), &opts).is_err());
        assert!(gcc.cxx_module_flags(&opts).is_err());
    }
}
