//! `--target` support: triple parsing plus toolchain selection.
//!
//! A requested triple resolves to an installed toolchain: triples matching
//! the host's architecture + OS reuse normal detection ([`select_for_target`]
//! → [`super::detect`]); anything else (a different OS, or a different
//! architecture such as `-m32`-style 32-bit on a 64-bit host) compiles with
//! `clang++ --target=<triple>` when clang is installed, and is otherwise an
//! actionable [`super::Error::NoCrossToolchain`] error naming what is
//! missing. Vendor/env components do not affect selection (only the
//! fingerprint) — see [`TargetTriple::is_native`].
//!
//! The target is recorded in the fingerprint via
//! [`super::CxxToolchain::fingerprint_with_target`]; the plain
//! [`super::CxxToolchain::fingerprint`] is untouched so native builds never
//! rebuild spuriously.

use std::ffi::OsString;
use std::path::PathBuf;

use super::{CxxToolchain, Error, ToolchainKind};

/// A parsed `--target` triple (`arch-vendor-os[-env]`), e.g.
/// `x86_64-pc-windows-msvc`, plus the `native` alias for the host triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetTriple {
    /// Canonical lowercase `arch-vendor-os[-env]` text (aliases normalized).
    pub raw: String,
    /// Normalized architecture (`x86_64`, `x86`, `aarch64`, `arm`,
    /// `riscv64`, `wasm32`).
    pub arch: String,
    /// Vendor as written, lowercased (`pc`, `unknown`, `apple`, …).
    pub vendor: String,
    /// Normalized OS (`windows`, `linux`, `darwin`, `freebsd`, `none`,
    /// `unknown`, `wasi`, `android`, `ios`, …).
    pub os: String,
    /// Environment/ABI when present (`gnu`, `msvc`, `musl`, …).
    pub env: Option<String>,
}

impl std::fmt::Display for TargetTriple {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.raw)
    }
}

impl TargetTriple {
    /// Parse a triple (`arch-vendor-os[-env]`) or `native` (any case).
    ///
    /// Architecture aliases (`x64`/`amd64` → `x86_64`, `arm64` → `aarch64`,
    /// `i686` → `x86`) and the `macos` → `darwin` OS alias are normalized so
    /// equivalent spellings share one fingerprint string. Anything else
    /// unknown is an actionable error listing what is supported.
    pub fn parse(input: &str) -> Result<Self, Error> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(Error::InvalidTarget {
                input: input.to_string(),
                reason: "empty string".to_string(),
            });
        }
        if trimmed.eq_ignore_ascii_case("native") {
            return Ok(Self::host());
        }
        let lower = trimmed.to_ascii_lowercase();
        let parts: Vec<&str> = lower.split('-').collect();
        if parts.len() < 3 || parts.len() > 4 || parts.iter().any(|p| p.is_empty()) {
            return Err(Error::InvalidTarget {
                input: input.to_string(),
                reason: format!(
                    "expected `arch-vendor-os[-env]` (3 or 4 `-`-separated parts), got {} (e.g. `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`)",
                    parts.len()
                ),
            });
        }
        let arch = normalize_arch(parts[0]).map_err(|reason| Error::InvalidTarget {
            input: input.to_string(),
            reason,
        })?;
        let vendor = parts[1].to_string();
        let os = normalize_os(parts[2]).map_err(|reason| Error::InvalidTarget {
            input: input.to_string(),
            reason,
        })?;
        let env = if parts.len() == 4 {
            Some(
                normalize_env(parts[3]).map_err(|reason| Error::InvalidTarget {
                    input: input.to_string(),
                    reason,
                })?,
            )
        } else {
            None
        };
        let raw = match &env {
            Some(e) => format!("{arch}-{vendor}-{os}-{e}"),
            None => format!("{arch}-{vendor}-{os}"),
        };
        Ok(Self {
            raw,
            arch,
            vendor,
            os,
            env,
        })
    }

    /// The host triple (`native`), e.g. `x86_64-pc-windows-msvc` on
    /// 64-bit Windows with MSVC ABI, `x86_64-unknown-linux-gnu` on typical
    /// Linux, `aarch64-apple-darwin` on Apple silicon. Best-effort on exotic
    /// hosts (unknown OS/arch pass through verbatim).
    pub fn host() -> Self {
        // `consts::ARCH` already uses our normalized names on supported
        // hosts; exotic values pass through verbatim.
        let arch = std::env::consts::ARCH;
        let (vendor, os, env): (&str, &str, Option<String>) = match std::env::consts::OS {
            "windows" => ("pc", "windows", Some("msvc".to_string())),
            "macos" => ("apple", "darwin", None),
            "linux" => ("unknown", "linux", Some("gnu".to_string())),
            other => ("unknown", other, None),
        };
        let raw = match &env {
            Some(e) => format!("{arch}-{vendor}-{os}-{e}"),
            None => format!("{arch}-{vendor}-{os}"),
        };
        Self {
            raw,
            arch: arch.to_string(),
            vendor: vendor.to_string(),
            os: os.to_string(),
            env,
        }
    }

    /// `true` when this triple targets the host machine: same architecture
    /// and OS. Vendor and env are ignored — e.g. `x86_64-pc-windows-gnu` on
    /// an MSVC Windows host still selects the native toolchain (the env
    /// difference is recorded in the fingerprint but needs no cross
    /// compiler; document any future need for env-specific selection here).
    pub fn is_native(&self) -> bool {
        let host = Self::host();
        self.arch == host.arch && self.os == host.os
    }
}

/// Normalize an architecture component (aliases included).
fn normalize_arch(arch: &str) -> Result<String, String> {
    let normalized = match arch {
        "x86_64" | "amd64" | "x64" => "x86_64",
        "i386" | "i486" | "i586" | "i686" | "x86" => "x86",
        "aarch64" | "arm64" => "aarch64",
        "arm" | "armv7" | "armv7l" => "arm",
        "riscv64" | "riscv64gc" => "riscv64",
        "wasm32" => "wasm32",
        _ => {
            return Err(format!(
                "unknown architecture `{arch}` (supported: x86, x86_64, aarch64, arm, riscv64, wasm32)"
            ));
        }
    };
    Ok(normalized.to_string())
}

/// Normalize an OS component (`macos` → `darwin`).
fn normalize_os(os: &str) -> Result<String, String> {
    let normalized = match os {
        "windows" => "windows",
        "linux" => "linux",
        "darwin" | "macos" => "darwin",
        "freebsd" => "freebsd",
        "openbsd" => "openbsd",
        "netbsd" => "netbsd",
        "none" => "none",
        "unknown" => "unknown",
        "wasi" => "wasi",
        "android" => "android",
        "ios" => "ios",
        _ => {
            return Err(format!(
                "unknown OS `{os}` (supported: windows, linux, darwin, freebsd, openbsd, netbsd, none, unknown, wasi, android, ios)"
            ));
        }
    };
    Ok(normalized.to_string())
}

/// Validate an environment/ABI component.
fn normalize_env(env: &str) -> Result<String, String> {
    match env {
        "gnu" | "gnueabi" | "gnueabihf" | "msvc" | "musl" | "android" | "eabi" | "eabihf"
        | "uclibc" => Ok(env.to_string()),
        _ => Err(format!(
            "unknown environment `{env}` (supported: gnu, gnueabi, gnueabihf, msvc, musl, android, eabi, eabihf, uclibc)"
        )),
    }
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

/// A resolved toolchain for a `--target` request.
#[derive(Debug, Clone)]
pub struct TargetSelection {
    /// Toolchain to compile with (detected native, or clang for cross).
    pub toolchain: CxxToolchain,
    /// Requested triple (`None` = default native detection).
    pub requested: Option<TargetTriple>,
    /// Extra driver args for cross builds (`["--target=<triple>"]`; empty
    /// for native). The coordinator appends these to compile and link
    /// commands.
    pub cross_args: Vec<OsString>,
    /// `true` when a cross compiler was selected.
    pub is_cross: bool,
}

impl TargetSelection {
    /// Fingerprint suffix for the selection (`|target=native` by default).
    /// The coordinator appends this to the toolchain fingerprint so target
    /// switches rebuild.
    pub fn target_suffix(&self) -> String {
        match &self.requested {
            None => "|target=native".to_string(),
            Some(t) => format!("|target={}", t.raw),
        }
    }

    /// Extra driver args to add to compile and link commands.
    pub fn compile_args(&self) -> &[OsString] {
        &self.cross_args
    }
}

/// Pure selection decision (no detection): native requests keep `native`,
/// cross requests need `clang`.
fn decide(
    requested: Option<TargetTriple>,
    native: CxxToolchain,
    clang: Option<CxxToolchain>,
) -> Result<TargetSelection, Error> {
    match requested {
        None => Ok(TargetSelection {
            toolchain: native,
            requested: None,
            cross_args: Vec::new(),
            is_cross: false,
        }),
        Some(triple) if triple.is_native() => Ok(TargetSelection {
            toolchain: native,
            requested: Some(triple),
            cross_args: Vec::new(),
            is_cross: false,
        }),
        Some(triple) => match clang {
            Some(c) => {
                let flag = OsString::from(format!("--target={}", triple.raw));
                Ok(TargetSelection {
                    toolchain: c,
                    requested: Some(triple),
                    cross_args: vec![flag],
                    is_cross: true,
                })
            }
            None => Err(Error::NoCrossToolchain {
                requested: triple.raw.clone(),
                host: TargetTriple::host().raw,
                tried: "clang++ in PATH: not found".to_string(),
            }),
        },
    }
}

/// Wrap a `clang++` path as a cross-capable toolchain.
fn clang_toolchain(path: PathBuf) -> CxxToolchain {
    let version = super::query_version(&path, &[OsString::from("--version")]);
    CxxToolchain::with_archiver(
        ToolchainKind::Clang,
        path,
        version,
        None,
        "clang++ in PATH (cross --target)".to_string(),
    )
}

/// Resolve a `--target` request to an installed toolchain.
///
/// - `None` (no flag) → normal detection, native suffix.
/// - A triple matching the host arch + OS → normal detection, with the
///   triple recorded in the suffix (one rebuild when first spelled out).
/// - Anything else → `clang++ --target=<triple>` when clang is installed,
///   else an actionable error naming the missing compiler.
///
/// Note: cross archiving still uses the clang toolchain's detected archiver
/// (native `ar`/`llvm-ar`); targets needing `x86_64-linux-gnu-ar`-style
/// prefixed binutils surface an actionable archiver error at archive time.
pub fn select_for_target(requested: Option<&TargetTriple>) -> Result<TargetSelection, Error> {
    let native = super::detect()?;
    let needs_cross = requested.is_some_and(|t| !t.is_native());
    let clang = if needs_cross {
        which::which("clang++").ok().map(clang_toolchain)
    } else {
        None
    };
    decide(requested.cloned(), native, clang)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(kind: ToolchainKind) -> CxxToolchain {
        CxxToolchain {
            kind,
            cxx: PathBuf::from("fake"),
            version: "test".to_string(),
            msvc_env: None,
            ar: None,
            target: None,
            found_via: "test".to_string(),
        }
    }

    #[test]
    fn parses_common_triples() {
        let cases: &[(&str, &str, &str, &str, Option<&str>)] = &[
            (
                "x86_64-pc-windows-msvc",
                "x86_64",
                "pc",
                "windows",
                Some("msvc"),
            ),
            (
                "x86_64-unknown-linux-gnu",
                "x86_64",
                "unknown",
                "linux",
                Some("gnu"),
            ),
            (
                "aarch64-unknown-linux-gnu",
                "aarch64",
                "unknown",
                "linux",
                Some("gnu"),
            ),
            ("x86_64-apple-darwin", "x86_64", "apple", "darwin", None),
            ("aarch64-apple-darwin", "aarch64", "apple", "darwin", None),
            (
                "x86_64-unknown-linux-musl",
                "x86_64",
                "unknown",
                "linux",
                Some("musl"),
            ),
            (
                "x86_64-pc-windows-gnu",
                "x86_64",
                "pc",
                "windows",
                Some("gnu"),
            ),
            ("x86-pc-windows-msvc", "x86", "pc", "windows", Some("msvc")),
            (
                "wasm32-unknown-unknown",
                "wasm32",
                "unknown",
                "unknown",
                None,
            ),
        ];
        for (input, arch, vendor, os, env) in cases {
            let t = TargetTriple::parse(input).unwrap();
            assert_eq!(&t.arch, arch, "input {input}");
            assert_eq!(&t.vendor, vendor, "input {input}");
            assert_eq!(&t.os, os, "input {input}");
            assert_eq!(t.env.as_deref(), *env, "input {input}");
            assert_eq!(&t.raw, input, "raw round-trips for {input}");
        }
    }

    #[test]
    fn normalizes_aliases() {
        assert_eq!(
            TargetTriple::parse("x64-pc-windows-msvc").unwrap().arch,
            "x86_64"
        );
        assert_eq!(
            TargetTriple::parse("ARM64-APPLE-DARWIN").unwrap().raw,
            "aarch64-apple-darwin"
        );
        assert_eq!(
            TargetTriple::parse("x86_64-apple-macos").unwrap().os,
            "darwin"
        );
        assert_eq!(
            TargetTriple::parse("i686-pc-windows-msvc").unwrap().raw,
            "x86-pc-windows-msvc"
        );
        assert_eq!(
            TargetTriple::parse("amd64-unknown-linux-gnu").unwrap().arch,
            "x86_64"
        );
    }

    #[test]
    fn native_alias_matches_host() {
        assert_eq!(TargetTriple::parse("native").unwrap(), TargetTriple::host());
        assert_eq!(TargetTriple::parse("NATIVE").unwrap(), TargetTriple::host());
        let host = TargetTriple::host();
        assert!(host.is_native());
        // The host triple re-parses to itself.
        assert_eq!(TargetTriple::parse(&host.raw).unwrap(), host);
    }

    #[test]
    fn rejects_bad_triples_actionably() {
        for bad in [
            "",
            "x86_64",
            "x86_64-linux",
            "x86_64-unknown-linux-gnu-extra-bit",
            "sparc-unknown-linux-gnu",
            "x86_64--linux-gnu",
            "x86_64-unknown-hurd",
            "x86_64-unknown-linux-superabi",
            "-unknown-linux-gnu",
        ] {
            let err = TargetTriple::parse(bad).unwrap_err().to_string();
            assert!(err.contains("target"), "input {bad:?} got: {err}");
            assert!(
                err.contains(bad) || err.contains("empty"),
                "input {bad:?} got: {err}"
            );
        }
    }

    #[test]
    fn native_requests_keep_native_toolchain() {
        let native = fake(ToolchainKind::Msvc);
        let sel = decide(None, native.clone(), None).unwrap();
        assert!(!sel.is_cross);
        assert!(sel.cross_args.is_empty());
        assert_eq!(sel.target_suffix(), "|target=native");

        let host = TargetTriple::host();
        let sel = decide(Some(host.clone()), native, None).unwrap();
        assert!(!sel.is_cross);
        assert_eq!(sel.target_suffix(), format!("|target={}", host.raw));
    }

    #[test]
    fn cross_needs_clang_else_actionable_error() {
        // A triple that cannot be native on any supported host.
        let foreign = if TargetTriple::host().os == "windows" {
            TargetTriple::parse("x86_64-unknown-linux-gnu").unwrap()
        } else {
            TargetTriple::parse("x86_64-pc-windows-msvc").unwrap()
        };
        assert!(!foreign.is_native());

        let err = decide(Some(foreign.clone()), fake(ToolchainKind::Gcc), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&foreign.raw), "got: {err}");
        assert!(err.contains("clang"), "got: {err}");

        let sel = decide(
            Some(foreign.clone()),
            fake(ToolchainKind::Gcc),
            Some(fake(ToolchainKind::Clang)),
        )
        .unwrap();
        assert!(sel.is_cross);
        assert_eq!(sel.toolchain.kind, ToolchainKind::Clang);
        assert_eq!(
            sel.compile_args(),
            &[OsString::from(format!("--target={}", foreign.raw))]
        );
        assert_eq!(sel.target_suffix(), format!("|target={}", foreign.raw));
    }

    #[test]
    fn m32_style_arch_difference_is_cross() {
        let host = TargetTriple::host();
        // Same OS, different arch: cross (the `-m32` situation).
        let other_arch = if host.arch == "x86_64" {
            "aarch64"
        } else {
            "x86_64"
        };
        let suffix = host.raw.split_once('-').map(|(_, rest)| rest).unwrap_or("");
        let triple = TargetTriple {
            raw: format!("{other_arch}-{suffix}"),
            arch: other_arch.to_string(),
            vendor: host.vendor.clone(),
            os: host.os.clone(),
            env: host.env.clone(),
        };
        assert!(!triple.is_native());
        let sel = decide(
            Some(triple),
            fake(ToolchainKind::Gcc),
            Some(fake(ToolchainKind::Clang)),
        )
        .unwrap();
        assert!(sel.is_cross);
    }

    #[test]
    fn live_native_selection_uses_detection() {
        // Needs a real toolchain (present on dev/CI machines, incl. MSVC here).
        let sel = select_for_target(None).unwrap();
        assert!(!sel.is_cross);
        assert_eq!(sel.target_suffix(), "|target=native");
        let host = TargetTriple::host();
        let sel = select_for_target(Some(&host)).unwrap();
        assert!(!sel.is_cross);
    }
}
