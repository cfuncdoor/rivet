//! Make adapter (best-effort): `make -C <src> [targets] [K=V] [-j N]`.
//!
//! Make has no out-of-source convention, so the build runs **in the
//! source dir** and outputs are discovered there too. The integrator must
//! hand this adapter a writable scratch copy when the real source is
//! immutable (a locked checkout or vendor copy), and should fingerprint
//! before building (new artifacts would otherwise perturb the hash).
//! Only the `make` binary is ever executed.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use rivet_manifest::AdapterSpec;

use super::discover::{discover_include_dirs, discover_libs, lib_dirs_of};
use super::util::{require_tool, run_capture};
use super::{AdapterCtx, AdapterOutput, Error};

/// Run `make` in `source_dir` per `spec`, discovering outputs there.
pub fn build(
    source_dir: &Path,
    spec: &AdapterSpec,
    ctx: &AdapterCtx,
) -> Result<AdapterOutput, Error> {
    let make_bin = require_tool("make")?;
    let mut cmd = Command::new(&make_bin);
    cmd.args(make_args(source_dir, spec, ctx));
    if let Some(tc) = &ctx.toolchain {
        tc.apply_env(&mut cmd);
    }
    run_capture(
        &mut cmd,
        &format!("build {} with make", source_dir.display()),
        "read the errors above — they come from the foreign project's own Makefile/sources",
        &[],
    )?;

    let libs = discover_libs(source_dir, &spec.targets)?;
    let lib_dirs = lib_dirs_of(&libs);
    let include_dirs = discover_include_dirs(source_dir, source_dir);
    Ok(AdapterOutput {
        include_dirs,
        lib_dirs,
        libs,
        defines: Vec::new(),
    })
}

/// `make -C src [targets …] [KEY=VALUE …] [-j N]`. Spec options become
/// make variables verbatim (no `-D` stripping: `KEY=VALUE` is make's own
/// syntax for variable overrides).
pub(crate) fn make_args(source_dir: &Path, spec: &AdapterSpec, ctx: &AdapterCtx) -> Vec<OsString> {
    let mut args = vec![OsString::from("-C"), source_dir.as_os_str().to_os_string()];
    for target in &spec.targets {
        args.push(OsString::from(target));
    }
    for (key, value) in &spec.options {
        args.push(OsString::from(format!("{key}={value}")));
    }
    if let Some(jobs) = ctx.jobs {
        args.push(OsString::from("-j"));
        args.push(OsString::from(jobs.to_string()));
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::tool_available;

    fn spec(targets: &[&str], options: &[(&str, &str)]) -> AdapterSpec {
        AdapterSpec {
            tool: "make".to_string(),
            targets: targets.iter().map(|s| s.to_string()).collect(),
            options: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn args_to_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn make_args_shape() {
        let ctx = AdapterCtx::new("build".into()).with_jobs(4);
        let args = args_to_strings(&make_args(
            Path::new("src"),
            &spec(&["all", "install"], &[("PREFIX", "/x"), ("CFLAGS", "-O2")]),
            &ctx,
        ));
        assert_eq!(
            args,
            vec![
                "-C",
                "src",
                "all",
                "install",
                "CFLAGS=-O2",
                "PREFIX=/x",
                "-j",
                "4"
            ],
            "got: {args:?}"
        );
        // Options stay sorted (BTreeMap) for deterministic fingerprints.
    }

    #[test]
    fn make_missing_or_makefileless_is_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = AdapterCtx::new(dir.path().join("build"));
        let err = build(dir.path(), &spec(&[], &[]), &ctx)
            .unwrap_err()
            .to_string();
        if !tool_available("make") {
            assert!(err.contains("make"), "got: {err}");
            assert!(err.contains("Fix:"), "got: {err}");
        } else {
            // `make` present but no Makefile: the tool's own error attached.
            assert!(err.contains("make"), "got: {err}");
        }
    }

    #[test]
    fn make_discovers_built_archive_in_source() {
        if !tool_available("make") {
            eprintln!("SKIP make_discovers_built_archive_in_source: make not on PATH");
            return;
        }
        let _dir = tempfile::tempdir().unwrap();
        // A Makefile that "builds" a fake archive via the shell only.
        // (Portability: skipped on Windows, where `make` is absent anyway.)
        #[cfg(not(windows))]
        {
            let dir = _dir;
            std::fs::write(
                dir.path().join("Makefile"),
                b"libmini.a:\n\tar crus libmini.a\n",
            )
            .unwrap();
            let ctx = AdapterCtx::new(dir.path().join("build"));
            let out = build(dir.path(), &spec(&["libmini.a"], &[]), &ctx).unwrap();
            assert_eq!(out.libs.len(), 1, "libs: {:?}", out.libs);
        }
    }
}
