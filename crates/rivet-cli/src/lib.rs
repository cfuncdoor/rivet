//! `rivet` command-line interface.
//!
//! Argument parsing (clap), Cargo-like colored output, progress reporting,
//! `--message-format=json` for IDEs, and `-v` verbose logging. M1 delivered
//! `new`, `init`, `build`, `run`, `clean`, and `toolchains`; M2 adds
//! workspace scope (`-p`, `--bin`, `-j/--jobs`, `--keep-going`) and
//! `Fresh`/`Compiling` per-package lines. Every other command parses and
//! returns an actionable "planned for Mx" error so the CLI shape is stable.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// Cargo-like build system and package manager for C++.
#[derive(Debug, Parser)]
#[command(name = "rivet", version, about = "Cargo-like build system for C++")]
pub struct Cli {
    /// Verbose output (log toolchain candidates, full compiler commands).
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Output format: `human` (default) or `json` (one JSON object per line).
    #[arg(long, global = true, default_value = "human", value_name = "FMT")]
    pub message_format: String,

    /// Color mode: `auto` (default), `always`, or `never`. `NO_COLOR` is honored.
    #[arg(long, global = true, default_value = "auto", value_name = "WHEN")]
    pub color: String,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// All `rivet` subcommands. Only M1 commands are implemented; the rest parse
/// successfully and fail with an actionable milestone pointer (D11).
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Create a new C++ package in a new directory.
    New {
        /// Package (and directory) name.
        name: String,
    },
    /// Create a new C++ package in the current directory.
    Init {
        /// Package name (defaults to the directory name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Compile the current package (or workspace).
    Build {
        /// Build with the `release` profile.
        #[arg(long)]
        release: bool,
        /// Build only the given package (plus its path deps).
        #[arg(short = 'p', long, value_name = "PKG")]
        package: Option<String>,
        /// Build only the given binary (plus its path deps).
        #[arg(long, value_name = "NAME")]
        bin: Option<String>,
        /// Parallel compile jobs (`0` = auto = cores).
        #[arg(short = 'j', long, default_value_t = 0, value_name = "N")]
        jobs: usize,
        /// Keep going after a compile failure (report all errors).
        #[arg(long)]
        keep_going: bool,
        /// Cross-compile for the given triple (M7).
        #[arg(long, value_name = "TRIPLE")]
        target: Option<String>,
        /// Do not use network (M4; accepted now, no network is used in M2).
        #[arg(long)]
        offline: bool,
        /// Error if the lockfile would change (M4; accepted now).
        #[arg(long)]
        locked: bool,
        /// No network + error if the lockfile would change (M4; accepted now).
        #[arg(long)]
        frozen: bool,
    },
    /// Build and run a binary.
    Run {
        /// Build with the `release` profile.
        #[arg(long)]
        release: bool,
        /// Run the given package's binary.
        #[arg(short = 'p', long, value_name = "PKG")]
        package: Option<String>,
        /// Run the given binary.
        #[arg(long, value_name = "NAME")]
        bin: Option<String>,
        /// Parallel compile jobs (`0` = auto = cores).
        #[arg(short = 'j', long, default_value_t = 0, value_name = "N")]
        jobs: usize,
        /// Keep going after a compile failure (report all errors).
        #[arg(long)]
        keep_going: bool,
        /// Arguments passed to the binary (after `--`).
        #[arg(last = true, value_name = "ARGS")]
        args: Vec<OsString>,
    },
    /// Remove `target/` build output.
    Clean {
        /// Clean only the given package.
        #[arg(short = 'p', long, value_name = "PKG")]
        package: Option<String>,
    },
    /// List detected C++ toolchains.
    Toolchains,
    /// Run tests (M3).
    Test {},
    /// Syntax-check without producing binaries (M3).
    Check {},
    /// Add a dependency (registry in M5, git in M4).
    Add {
        /// Dependency name, optionally with `@version`.
        pkg: String,
    },
    /// Remove a dependency (M5).
    Remove {
        /// Dependency name.
        pkg: String,
    },
    /// Re-resolve dependencies (M4).
    Update {
        /// Only update this package.
        #[arg(short = 'p', long, value_name = "PKG")]
        package: Option<String>,
    },
    /// Print the dependency tree (M4).
    Tree,
    /// Format sources via clang-format if found (M8).
    Fmt,
    /// Build documentation (stretch).
    Doc,
    /// Copy all deps into `vendor/` for offline builds (M7).
    Vendor,
    /// Publish the package to the registry (M5).
    Publish,
    /// Log in to the registry (M5).
    Login,
    /// Search the registry (M5).
    Search {
        /// Query string.
        query: Option<String>,
    },
    /// Build and copy binaries to `~/.rivet/bin` (M5).
    Install,
    /// Emit `compile_commands.json` (M3).
    CompileCommands,
    /// Git dependency management (M4).
    Git {
        #[command(subcommand)]
        command: GitCommands,
    },
    /// Check out a writable clone of a git dep (M4).
    Dev {
        /// Dependency name.
        name: String,
    },
}

/// `rivet git` subcommands (all M4).
#[derive(Debug, Subcommand)]
pub enum GitCommands {
    /// Show cached remotes, checkouts, sizes, and users.
    List,
    /// Delete checkouts/mirrors unreferenced by known lockfiles.
    Prune,
    /// Prefetch everything in the lockfile (CI warm-up).
    Fetch,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// CLI entry point (called by the thin `src/main.rs` wrapper).
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    dispatch(&cli)
}

/// Test seam: dispatch a parsed CLI.
pub fn dispatch(cli: &Cli) -> Result<()> {
    let ui = Ui::new(cli);
    if cli.verbose {
        ui.verbose("rivet verbose logging enabled (-v)");
    }
    match &cli.command {
        None => {
            // Bare `rivet` behaves like `cargo`: print help.
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
        Some(Commands::New { name }) => cmd_new(&ui, name),
        Some(Commands::Init { name }) => cmd_init(&ui, name.clone()),
        Some(Commands::Build {
            release,
            package,
            bin,
            jobs,
            keep_going,
            target,
            offline,
            locked,
            frozen,
        }) => cmd_build(
            &ui,
            BuildFlags {
                release: *release,
                package: package.clone(),
                bin: bin.clone(),
                jobs: *jobs,
                keep_going: *keep_going,
                target: target.clone(),
                verbose: cli.verbose,
                json: ui.json,
                offline: *offline,
                locked: *locked,
                frozen: *frozen,
            },
        )
        .map(|_| ()),
        Some(Commands::Run {
            release,
            package,
            bin,
            jobs,
            keep_going,
            args,
        }) => cmd_run(
            &ui,
            RunFlags {
                release: *release,
                package: package.clone(),
                bin: bin.clone(),
                jobs: *jobs,
                keep_going: *keep_going,
                args: args.clone(),
                verbose: cli.verbose,
                json: ui.json,
            },
        ),
        Some(Commands::Clean { package }) => cmd_clean(&ui, package.clone()),
        Some(Commands::Toolchains) => cmd_toolchains(&ui),
        Some(other) => bail!(unimplemented_message(other)),
    }
}

/// Public entry used by `src/main.rs` (parse + dispatch).
pub fn run_cli() -> Result<()> {
    run()
}

// ---------------------------------------------------------------------------
// Output helper
// ---------------------------------------------------------------------------

/// Output helper: Cargo-like `Status` lines, `-v` logging, JSON mode.
pub struct Ui {
    color: bool,
    /// Verbose logging enabled.
    pub verbose: bool,
    /// JSON message format enabled.
    pub json: bool,
}

impl Ui {
    /// Create a UI from CLI flags + environment (`NO_COLOR`, TTY).
    pub fn new(cli: &Cli) -> Self {
        let message_format = cli.message_format.to_ascii_lowercase();
        Self {
            color: use_color(&cli.color),
            verbose: cli.verbose,
            json: message_format == "json",
        }
    }

    /// `   Compiling hello v0.1.0 (…)` — green bold action, plain message.
    pub fn status(&self, action: &str, message: &str) {
        if self.json {
            let obj = serde_json::json!({"type":"status","action":action,"message":message});
            println!("{obj}");
        } else if self.color {
            println!("\x1b[1;32m{:>12}\x1b[0m {message}", action);
        } else {
            println!("{:>12} {message}", action);
        }
    }

    /// Error lines go to stderr, mirroring Cargo (`error: …`).
    pub fn error(&self, message: &str) {
        if self.json {
            let obj = serde_json::json!({"type":"error","message":message});
            eprintln!("{obj}");
        } else if self.color {
            eprintln!("\x1b[1;31m{:>12}\x1b[0m {message}", "error");
        } else {
            eprintln!("{:>12} {message}", "error");
        }
    }

    /// `-v` log line to stderr (`       Verbose …` is intentionally quiet).
    pub fn verbose(&self, message: &str) {
        if self.verbose {
            eprintln!("     verbose {message}");
        }
    }

    /// Warn about forward-compat situations (e.g. patch during publish later).
    pub fn warn(&self, message: &str) {
        if self.json {
            let obj = serde_json::json!({"type":"warning","message":message});
            eprintln!("{obj}");
        } else if self.color {
            eprintln!("\x1b[1;33m{:>12}\x1b[0m {message}", "warning");
        } else {
            eprintln!("{:>12} {message}", "warning");
        }
    }
}

fn use_color(when: &str) -> bool {
    match when.to_ascii_lowercase().as_str() {
        "always" => true,
        "never" => false,
        _ => {
            if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
                return false;
            }
            std::io::stdout().is_terminal()
        }
    }
}

fn unimplemented_message(cmd: &Commands) -> String {
    let (name, milestone) = match cmd {
        Commands::Test {} => ("test", "M3"),
        Commands::Check {} => ("check", "M3"),
        Commands::Add { .. } => ("add", "M4/M5"),
        Commands::Remove { .. } => ("remove", "M5"),
        Commands::Update { .. } => ("update", "M4"),
        Commands::Tree => ("tree", "M4"),
        Commands::Fmt => ("fmt", "M8"),
        Commands::Doc => ("doc", "M8"),
        Commands::Vendor => ("vendor", "M7"),
        Commands::Publish => ("publish", "M5"),
        Commands::Login => ("login", "M5"),
        Commands::Search { .. } => ("search", "M5"),
        Commands::Install => ("install", "M5"),
        Commands::CompileCommands => ("compile-commands", "M3"),
        Commands::Git { .. } => ("git", "M4"),
        Commands::Dev { .. } => ("dev", "M4"),
        _ => ("command", "a later milestone"),
    };
    format!("`rivet {name}` is not implemented yet (planned for {milestone}). See PLAN.md for the milestone checklist")
}

// ---------------------------------------------------------------------------
// Commands: new / init
// ---------------------------------------------------------------------------

fn validate_package_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!(
            "package name must not be empty. Fix: `rivet new <name>` with e.g. `rivet new hello`"
        );
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid package name `{name}`. Fix: use only [a-zA-Z0-9_-], e.g. `rivet new hello`");
    }
    Ok(())
}

fn manifest_template(name: &str) -> String {
    format!(
        r#"{{
  "$schema": "https://rivet.dev/schema/rivet.json",
  "package": {{
    "name": "{name}",
    "version": "0.1.0",
    "standard": "c++17"
  }}
}}
"#
    )
}

const MAIN_CPP_TEMPLATE: &str = r#"#include <iostream>

int main() {
    std::cout << "Hello, world!\n";
    return 0;
}
"#;

const GITIGNORE_TEMPLATE: &str = "/target\n";

fn scaffold_package(ui: &Ui, dir: &Path, name: &str) -> Result<()> {
    validate_package_name(name)?;
    std::fs::create_dir_all(dir.join("src"))
        .with_context(|| format!("could not create {}", dir.join("src").display()))?;
    let manifest_path = dir.join("rivet.json");
    if manifest_path.exists() {
        bail!(
            "{} already exists. Fix: remove it or pick another name",
            manifest_path.display()
        );
    }
    std::fs::write(&manifest_path, manifest_template(name))
        .with_context(|| format!("could not write {}", manifest_path.display()))?;
    let main_cpp = dir.join("src").join("main.cpp");
    if !main_cpp.exists() {
        std::fs::write(&main_cpp, MAIN_CPP_TEMPLATE)
            .with_context(|| format!("could not write {}", main_cpp.display()))?;
    }
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, GITIGNORE_TEMPLATE)?;
    }
    ui.status(
        "Created",
        &format!("binary package `{name}` in {}", dir.display()),
    );
    Ok(())
}

fn cmd_new(ui: &Ui, name: &str) -> Result<()> {
    validate_package_name(name)?;
    let dir = PathBuf::from(name);
    if dir.exists() {
        // Mirror Cargo: refuse unless the directory is empty.
        let empty = std::fs::read_dir(&dir)
            .map(|mut r| r.next().is_none())
            .unwrap_or(true);
        if !empty {
            bail!(
                "directory `{name}` already exists and is not empty. Fix: pick another name or remove it"
            );
        }
    }
    scaffold_package(ui, &dir, name)
}

fn cmd_init(ui: &Ui, name: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir().context("could not read current directory")?;
    let default_name = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("hello")
        .to_string();
    let name = name.unwrap_or(default_name);
    scaffold_package(ui, &cwd, &name)
}

// ---------------------------------------------------------------------------
// Commands: build / run / clean / toolchains
// ---------------------------------------------------------------------------

/// Flags for [`cmd_build`].
pub struct BuildFlags {
    /// Use the `release` profile.
    pub release: bool,
    /// `-p` package selector (plus its path deps).
    pub package: Option<String>,
    /// `--bin` binary selector (plus its path deps).
    pub bin: Option<String>,
    /// Parallel compile jobs (`0` = auto).
    pub jobs: usize,
    /// Report all compile errors instead of stopping at the first.
    pub keep_going: bool,
    /// `--target` triple (cross-compile, M7).
    pub target: Option<String>,
    /// Verbose logging.
    pub verbose: bool,
    /// JSON message format.
    pub json: bool,
    /// Accepted now (no network is used in M2); enforced from M4.
    pub offline: bool,
    /// Accepted now; enforced from M4.
    pub locked: bool,
    /// Accepted now; enforced from M4.
    pub frozen: bool,
}

/// Flags for [`cmd_run`].
pub struct RunFlags {
    /// Use the `release` profile.
    pub release: bool,
    /// `-p` package selector.
    pub package: Option<String>,
    /// `--bin` binary selector.
    pub bin: Option<String>,
    /// Parallel compile jobs (`0` = auto).
    pub jobs: usize,
    /// Report all compile errors instead of stopping at the first.
    pub keep_going: bool,
    /// Arguments forwarded to the binary.
    pub args: Vec<OsString>,
    /// Verbose logging.
    pub verbose: bool,
    /// JSON message format.
    pub json: bool,
}

/// One built unit: its resolution, plan, and (if it was stale) summary.
struct BuiltUnit {
    unit: rivet_graph::workspace::Unit,
    plan: rivet_graph::PackagePlan,
}

/// Find the workspace scope root: the topmost ancestor of `nearest` that
/// still contains a `rivet.json`. (Cargo behavior: commands run from a
/// member operate on the whole workspace unless `-p` narrows.)
fn find_scope_root(nearest: &Path) -> PathBuf {
    let mut root = nearest.to_path_buf();
    let mut cursor = nearest.parent().map(Path::to_path_buf);
    while let Some(dir) = cursor {
        if dir.join("rivet.json").is_file() {
            root = dir.clone();
            cursor = dir.parent().map(Path::to_path_buf);
        } else {
            break;
        }
    }
    root
}

/// Load the scope root manifest for the current directory.
fn load_scope() -> Result<rivet_manifest::Manifest> {
    let cwd = std::env::current_dir().context("could not read current directory")?;
    let nearest = rivet_manifest::find_manifest_dir(&cwd).map_err(|e| anyhow::anyhow!("{e}"))?;
    let scope_root = find_scope_root(&nearest);
    rivet_manifest::load(&scope_root).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Restrict resolved units to the `--bin` target plus its transitive path
/// deps (topo order preserved).
fn restrict_to_bin(
    units: Vec<rivet_graph::workspace::Unit>,
    bin: &str,
) -> Result<Vec<rivet_graph::workspace::Unit>> {
    use std::collections::BTreeSet;
    // Candidate bins across the selection.
    let mut hits: Vec<String> = Vec::new();
    for unit in &units {
        for target in rivet_manifest::resolve_bins(&unit.manifest) {
            if target.name == *bin {
                if let Some(name) = rivet_manifest::package_name(&unit.manifest) {
                    hits.push(name.to_string());
                }
            }
        }
    }
    if hits.is_empty() {
        bail!(
            "binary `{bin}` not found in the selected packages. Fix: check `bin` entries and `src/main.cpp` files"
        );
    }
    if hits.len() > 1 {
        hits.sort();
        hits.dedup();
        bail!(
            "binary `{bin}` exists in multiple packages ({}). Fix: disambiguate with `-p <package> --bin {bin}`",
            hits.join(", ")
        );
    }
    // Keep the owning unit plus its transitive deps.
    let owner = units
        .iter()
        .find(|u| {
            rivet_manifest::resolve_bins(&u.manifest)
                .iter()
                .any(|t| t.name == *bin)
        })
        .unwrap()
        .dir
        .clone();
    let mut keep: BTreeSet<PathBuf> = BTreeSet::new();
    let mut stack = vec![owner];
    while let Some(dir) = stack.pop() {
        if !keep.insert(dir.clone()) {
            continue;
        }
        if let Some(unit) = units.iter().find(|u| u.dir == dir) {
            stack.extend(unit.deps.iter().cloned());
        }
    }
    Ok(units
        .into_iter()
        .filter(|u| keep.contains(&u.dir))
        .collect())
}

/// Build every unit in scope, returning them with their plans.
/// Prints Cargo-like `Compiling`/`Fresh`/`Finished` lines.
fn cmd_build(ui: &Ui, flags: BuildFlags) -> Result<Vec<BuiltUnit>> {
    if let Some(triple) = &flags.target {
        bail!("cross-compiling to `{triple}` is not implemented yet (planned for M7). See PLAN.md");
    }
    let root = load_scope()?;
    let profile = if flags.release { "release" } else { "dev" };
    let mut units = rivet_graph::workspace::resolve_units(&root, profile, flags.package.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(bin) = &flags.bin {
        units = restrict_to_bin(units, bin)?;
    }

    let detection = rivet_toolchain::detect().map_err(|e| anyhow::anyhow!("{e}"))?;
    if flags.verbose {
        ui.verbose(&format!(
            "toolchain: {} {} ({})",
            detection.kind,
            detection.cxx.display(),
            detection.version
        ));
        ui.verbose(&format!("found via: {}", detection.found_via));
        if let Some(ar) = &detection.ar {
            ui.verbose(&format!("archiver: {}", ar.display()));
        }
    }
    let build_opts = rivet_graph::BuildOptions {
        verbose: flags.verbose,
        jobs: flags.jobs,
        fail_fast: !flags.keep_going,
    };
    let started = std::time::Instant::now();
    let mut built = Vec::new();
    for unit in &units {
        let name = rivet_manifest::package_name(&unit.manifest).unwrap_or("<unknown>");
        let version = rivet_manifest::package_version(&unit.manifest).unwrap_or("0.0.0");
        let plan = rivet_graph::plan_with(&unit.manifest, profile, &detection)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let inputs = rivet_graph::workspace::unit_inputs(unit, &units, profile, &detection)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if rivet_graph::is_fresh_unit(&unit.manifest, &plan, &detection, &inputs)
            .map_err(|e| anyhow::anyhow!("{e}"))?
        {
            ui.status("Fresh", &format!("{name} v{version}"));
        } else {
            ui.status("Compiling", &format!("{name} v{version} ({profile})"));
            let summary =
                rivet_graph::build_unit(&unit.manifest, &plan, &detection, &inputs, &build_opts)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            for line in &summary.log {
                eprintln!("       {line}");
            }
            for (tag, rel) in &summary.recompiled {
                ui.verbose(&format!("recompiled {tag}/{rel}"));
            }
            for out in &summary.relinked {
                ui.verbose(&format!("linked {}", out.display()));
            }
        }
        built.push(BuiltUnit {
            unit: unit.clone(),
            plan,
        });
    }
    let secs = started.elapsed().as_secs_f64();
    ui.status("Finished", &format!("{profile} profile in {secs:.2}s"));
    Ok(built)
}

fn cmd_run(ui: &Ui, flags: RunFlags) -> Result<()> {
    let built = cmd_build(
        ui,
        BuildFlags {
            release: flags.release,
            package: flags.package.clone(),
            bin: flags.bin.clone(),
            jobs: flags.jobs,
            keep_going: flags.keep_going,
            target: None,
            verbose: flags.verbose,
            json: flags.json,
            offline: false,
            locked: false,
            frozen: false,
        },
    )?;
    // Select the binary: `--bin` narrows (already unique), else the single
    // bin in scope, else an actionable list.
    let mut bins: Vec<(&rivet_graph::workspace::Unit, &rivet_graph::TargetBuild)> = Vec::new();
    for b in &built {
        for target in &b.plan.bins {
            if let Some(want) = &flags.bin {
                if target.name == *want {
                    bins.push((&b.unit, target));
                }
            } else {
                bins.push((&b.unit, target));
            }
        }
    }
    if bins.is_empty() {
        bail!(
            "no binary targets found. Fix: create src/main.cpp or add a `bin` entry to rivet.json"
        );
    }
    let (unit, bin) = if bins.len() == 1 {
        bins.into_iter().next().unwrap()
    } else {
        let mut candidates: Vec<String> = bins
            .iter()
            .map(|(u, t)| {
                format!(
                    "{} (-p {})",
                    t.name,
                    rivet_manifest::package_name(&u.manifest).unwrap_or("?")
                )
            })
            .collect();
        candidates.sort();
        candidates.dedup();
        bail!(
            "multiple binaries available: {}. Fix: select one with `rivet run --bin <name>` or `rivet run -p <package>`",
            candidates.join(", ")
        );
    };
    ui.status("Running", &format!("{}", bin.output.display()));
    let mut cmd = std::process::Command::new(&bin.output);
    cmd.args(&flags.args);
    cmd.current_dir(&unit.dir);
    let status = cmd.status().with_context(|| {
        format!(
            "could not execute {} (was it built? run `rivet build` with -v for detail)",
            bin.output.display()
        )
    })?;
    if !status.success() {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        bail!("process exited with status {code}");
    }
    Ok(())
}

fn cmd_clean(ui: &Ui, package: Option<String>) -> Result<()> {
    let root = load_scope()?;
    // Scope packages without resolving deps (clean must not fail on
    // registry/git deps it never touches).
    let mut manifests = vec![root];
    if manifests[0].workspace.is_some() {
        let members = manifests[0].workspace.as_ref().unwrap().members.clone();
        let root_dir = manifests[0].dir.clone();
        for dir in rivet_manifest::expand_members(&root_dir, &members)
            .map_err(|e| anyhow::anyhow!("{e}"))?
        {
            manifests.push(rivet_manifest::load(&dir).map_err(|e| anyhow::anyhow!("{e}"))?);
        }
    }
    if let Some(name) = &package {
        manifests.retain(|m| rivet_manifest::package_name(m) == Some(name.as_str()));
        if manifests.is_empty() {
            bail!("package `{name}` not found in this workspace. Fix: use `-p` with an available package");
        }
    }
    for manifest in &manifests {
        let target = manifest.dir.join("target");
        if target.exists() {
            std::fs::remove_dir_all(&target)
                .with_context(|| format!("could not remove {}", target.display()))?;
            ui.status("Removed", &format!("{}", target.display()));
        } else {
            ui.status(
                "Removed",
                &format!(
                    "nothing to clean in {} (no target/ directory)",
                    manifest.dir.display()
                ),
            );
        }
    }
    Ok(())
}

fn cmd_toolchains(ui: &Ui) -> Result<()> {
    let infos = rivet_toolchain::list_toolchains();
    if infos.is_empty() {
        ui.error("no C++ toolchains detected. Install VS Build Tools, LLVM, or gcc; see `rivet build` error text");
        bail!("no C++ toolchain found");
    }
    for info in &infos {
        let marker = if info.is_default { " (default)" } else { "" };
        ui.status(
            "Toolchain",
            &format!(
                "{} {} — {}{marker}",
                info.kind,
                info.path.display(),
                info.version
            ),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_package_names() {
        assert!(validate_package_name("hello").is_ok());
        assert!(validate_package_name("my-app_2").is_ok());
        assert!(validate_package_name("").is_err());
        assert!(validate_package_name("has space").is_err());
        assert!(validate_package_name("semi;colon").is_err());
    }

    #[test]
    fn manifest_template_parses() {
        let text = manifest_template("hello");
        let m = rivet_manifest::parse_str(&text).unwrap();
        assert_eq!(rivet_manifest::package_name(&m), Some("hello"));
    }

    #[test]
    fn scope_root_stops_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        // No rivet.json anywhere above: scope root is the start dir itself.
        assert_eq!(find_scope_root(dir.path()), dir.path().to_path_buf());
    }

    #[test]
    fn unimplemented_commands_point_at_milestones() {
        let msg = unimplemented_message(&Commands::Test {});
        assert!(msg.contains("M3"), "got: {msg}");
        let msg = unimplemented_message(&Commands::Git {
            command: GitCommands::List,
        });
        assert!(msg.contains("M4"), "got: {msg}");
        let msg = unimplemented_message(&Commands::Publish);
        assert!(msg.contains("M5"), "got: {msg}");
    }

    #[test]
    fn no_color_env_disables_color() {
        let prev = std::env::var_os("NO_COLOR");
        std::env::set_var("NO_COLOR", "1");
        assert!(!use_color("auto"));
        assert!(use_color("always"));
        match prev {
            Some(v) => std::env::set_var("NO_COLOR", v),
            None => std::env::remove_var("NO_COLOR"),
        }
    }
}
