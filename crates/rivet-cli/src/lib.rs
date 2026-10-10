//! `rivet` command-line interface.
//!
//! Argument parsing (clap), Cargo-like colored output, progress reporting,
//! `--message-format=json` for IDEs, and `-v` verbose logging. Unimplemented
//! commands parse and fail with an actionable error so the CLI shape stays
//! stable while the feature set grows.

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

/// Lockfile flow (resolve-from-lock, update, vendor, dev, git cache).
mod lockflow;

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

/// All `rivet` subcommands. Commands that are not implemented yet parse
/// successfully and fail with an actionable error.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Create a new C++ package in a new directory.
    New {
        /// Package (and directory) name.
        name: String,
        /// Scaffold a static library (`src/lib.cpp` + `include/<name>.h`).
        #[arg(long, conflicts_with = "shared")]
        lib: bool,
        /// Scaffold a shared library (implies `--lib` with `type = "shared"`).
        #[arg(long, conflicts_with = "lib")]
        shared: bool,
    },
    /// Create a new C++ package in the current directory.
    Init {
        /// Package name (defaults to the directory name).
        #[arg(long)]
        name: Option<String>,
        /// Scaffold a static library (`src/lib.cpp` + `include/<name>.h`).
        #[arg(long, conflicts_with = "shared")]
        lib: bool,
        /// Scaffold a shared library (implies `--lib` with `type = "shared"`).
        #[arg(long, conflicts_with = "lib")]
        shared: bool,
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
        /// Enable features (comma-separated, repeatable).
        #[arg(long, value_delimiter = ',', value_name = "FEATURES")]
        features: Vec<String>,
        /// Enable all features.
        #[arg(long)]
        all_features: bool,
        /// Disable the `default` feature.
        #[arg(long)]
        no_default_features: bool,
        /// Cross-compile for the given triple (`native` = host default).
        #[arg(long, value_name = "TRIPLE")]
        target: Option<String>,
        /// Do not use network (cache/vendor only).
        #[arg(long)]
        offline: bool,
        /// Error if the lockfile would change.
        #[arg(long)]
        locked: bool,
        /// No network + error if the lockfile would change.
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
        /// Enable features (comma-separated, repeatable).
        #[arg(long, value_delimiter = ',', value_name = "FEATURES")]
        features: Vec<String>,
        /// Enable all features.
        #[arg(long)]
        all_features: bool,
        /// Disable the `default` feature.
        #[arg(long)]
        no_default_features: bool,
        /// Cross-compile for the given triple (`native` = host default).
        #[arg(long, value_name = "TRIPLE")]
        target: Option<String>,
        /// Do not use network (cache/vendor only).
        #[arg(long)]
        offline: bool,
        /// Error if the lockfile would change.
        #[arg(long)]
        locked: bool,
        /// No network + error if the lockfile would change.
        #[arg(long)]
        frozen: bool,
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
    /// Run tests.
    Test {
        /// Test with the `release` profile.
        #[arg(long)]
        release: bool,
        /// Test only the given package (plus its path deps).
        #[arg(short = 'p', long, value_name = "PKG")]
        package: Option<String>,
        /// Parallel compile jobs (`0` = auto = cores).
        #[arg(short = 'j', long, default_value_t = 0, value_name = "N")]
        jobs: usize,
        /// Keep going after a compile failure (report all errors).
        #[arg(long)]
        keep_going: bool,
        /// Enable features (comma-separated, repeatable).
        #[arg(long, value_delimiter = ',', value_name = "FEATURES")]
        features: Vec<String>,
        /// Enable all features.
        #[arg(long)]
        all_features: bool,
        /// Disable the `default` feature.
        #[arg(long)]
        no_default_features: bool,
        /// Cross-compile for the given triple (`native` = host default).
        #[arg(long, value_name = "TRIPLE")]
        target: Option<String>,
        /// Do not use network (cache/vendor only).
        #[arg(long)]
        offline: bool,
        /// Error if the lockfile would change.
        #[arg(long)]
        locked: bool,
        /// No network + error if the lockfile would change.
        #[arg(long)]
        frozen: bool,
    },
    /// Syntax-check without producing binaries.
    Check {},
    /// Add a dependency (`name[@version]`, `owner/repo[@ref]`, or git URL).
    Add {
        /// Dependency spec, optionally with `@version`/`@ref`.
        pkg: String,
    },
    /// Remove a dependency.
    Remove {
        /// Dependency name.
        pkg: String,
    },
    /// Re-resolve dependencies.
    Update {
        /// Only update this package (others keep their locked entries).
        #[arg(short = 'p', long, value_name = "PKG")]
        package: Option<String>,
        /// Pin the selected package to an exact commit SHA (requires `-p`).
        #[arg(long, value_name = "SHA")]
        precise: Option<String>,
        /// No network (mirrors only).
        #[arg(long)]
        offline: bool,
    },
    /// Print the dependency tree (locked versions when rivet.lock.json exists).
    Tree,
    /// Format sources via clang-format if found.
    Fmt {
        /// Check formatting without writing (exit nonzero on diffs).
        #[arg(long)]
        check: bool,
    },
    /// Build documentation (stretch).
    Doc,
    /// Copy all deps into `vendor/` for offline builds.
    Vendor {
        /// Show what would be vendored without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Publish the package to the registry.
    Publish {
        /// Registry URL (default: `RIVET_REGISTRY`).
        #[arg(long, value_name = "URL")]
        registry: Option<String>,
        /// Auth token (default: prompt-avoiding `RIVET_TOKEN` env or stdin).
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        /// Show what would be uploaded without uploading.
        #[arg(long)]
        dry_run: bool,
        /// Publish only the given workspace package.
        #[arg(short = 'p', long, value_name = "PKG")]
        package: Option<String>,
    },
    /// Log in to the registry (stores a token for `publish`).
    Login {
        /// Registry URL (default: `RIVET_REGISTRY`).
        #[arg(long, value_name = "URL")]
        registry: Option<String>,
        /// Auth token (default: read one line from stdin).
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
    },
    /// Search the registry index.
    Search {
        /// Query string.
        query: Option<String>,
        /// Registry URL (default: `RIVET_REGISTRY` or the local index copy).
        #[arg(long, value_name = "URL")]
        registry: Option<String>,
    },
    /// Build and copy binaries to `~/.rivet/bin`.
    Install {
        /// Install only the given package (plus its path deps).
        #[arg(short = 'p', long, value_name = "PKG")]
        package: Option<String>,
        /// Install only the given binary.
        #[arg(long, value_name = "NAME")]
        bin: Option<String>,
        /// Build with the `release` profile.
        #[arg(long)]
        release: bool,
        /// Parallel compile jobs (`0` = auto = cores).
        #[arg(short = 'j', long, default_value_t = 0, value_name = "N")]
        jobs: usize,
        /// Enable features (comma-separated, repeatable).
        #[arg(long, value_delimiter = ',', value_name = "FEATURES")]
        features: Vec<String>,
        /// Enable all features.
        #[arg(long)]
        all_features: bool,
        /// Disable the `default` feature.
        #[arg(long)]
        no_default_features: bool,
        /// Do not use network (cache/vendor only).
        #[arg(long)]
        offline: bool,
        /// Error if the lockfile would change.
        #[arg(long)]
        locked: bool,
        /// No network + error if the lockfile would change.
        #[arg(long)]
        frozen: bool,
    },
    /// Emit `compile_commands.json`.
    CompileCommands {
        /// Use the `release` profile flags.
        #[arg(long)]
        release: bool,
        /// Emit only for the given package (plus its path deps).
        #[arg(short = 'p', long, value_name = "PKG")]
        package: Option<String>,
        /// Enable features (comma-separated, repeatable).
        #[arg(long, value_delimiter = ',', value_name = "FEATURES")]
        features: Vec<String>,
        /// Enable all features.
        #[arg(long)]
        all_features: bool,
        /// Disable the `default` feature.
        #[arg(long)]
        no_default_features: bool,
        /// Cross-compile for the given triple (`native` = host default).
        #[arg(long, value_name = "TRIPLE")]
        target: Option<String>,
        /// Do not use network (cache/vendor only).
        #[arg(long)]
        offline: bool,
        /// Error if the lockfile would change.
        #[arg(long)]
        locked: bool,
        /// No network + error if the lockfile would change.
        #[arg(long)]
        frozen: bool,
    },
    /// Git dependency management.
    Git {
        #[command(subcommand)]
        command: GitCommands,
    },
    /// Check out a writable clone of a git dep.
    Dev {
        /// Dependency name.
        name: String,
    },
    /// Print shell completions (`bash`, `zsh`, `fish`, `powershell`, `elvish`).
    Completions {
        /// Shell name.
        shell: String,
    },
}

/// `rivet git` subcommands.
#[derive(Debug, Subcommand)]
pub enum GitCommands {
    /// Show cached remotes, checkouts, sizes, and users.
    List,
    /// Delete checkouts/mirrors unreferenced by known lockfiles.
    Prune {
        /// Show what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
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
        Some(Commands::New { name, lib, shared }) => {
            cmd_new(&ui, name, NewKind::resolve(*lib, *shared)?)
        }
        Some(Commands::Init { name, lib, shared }) => {
            cmd_init(&ui, name.clone(), NewKind::resolve(*lib, *shared)?)
        }
        Some(Commands::Build {
            release,
            package,
            bin,
            jobs,
            keep_going,
            features,
            all_features,
            no_default_features,
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
                features: feature_flags(features, *all_features, *no_default_features),
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
            features,
            all_features,
            no_default_features,
            target,
            offline,
            locked,
            frozen,
            args,
        }) => cmd_run(
            &ui,
            RunFlags {
                release: *release,
                package: package.clone(),
                bin: bin.clone(),
                jobs: *jobs,
                keep_going: *keep_going,
                features: feature_flags(features, *all_features, *no_default_features),
                target: target.clone(),
                args: args.clone(),
                verbose: cli.verbose,
                json: ui.json,
                offline: *offline,
                locked: *locked,
                frozen: *frozen,
            },
        ),
        Some(Commands::Test {
            release,
            package,
            jobs,
            keep_going,
            features,
            all_features,
            no_default_features,
            target,
            offline,
            locked,
            frozen,
        }) => cmd_test(
            &ui,
            TestFlags {
                release: *release,
                package: package.clone(),
                jobs: *jobs,
                keep_going: *keep_going,
                features: feature_flags(features, *all_features, *no_default_features),
                target: target.clone(),
                verbose: cli.verbose,
                json: ui.json,
                offline: *offline,
                locked: *locked,
                frozen: *frozen,
            },
        ),
        Some(Commands::CompileCommands {
            release,
            package,
            features,
            all_features,
            no_default_features,
            target,
            offline,
            locked,
            frozen,
        }) => cmd_compile_commands(
            &ui,
            CompileCommandsFlags {
                release: *release,
                package: package.clone(),
                features: feature_flags(features, *all_features, *no_default_features),
                target: target.clone(),
                verbose: cli.verbose,
                offline: *offline,
                locked: *locked,
                frozen: *frozen,
            },
        ),
        Some(Commands::Update {
            package,
            precise,
            offline,
        }) => cmd_update(
            &ui,
            lockflow::UpdateOpts {
                package: package.clone(),
                precise: precise.clone(),
                offline: *offline,
            },
            cli.verbose,
        ),
        Some(Commands::Git { command }) => cmd_git(&ui, command, cli.verbose),
        Some(Commands::Add { pkg }) => {
            let root = load_scope()?;
            lockflow::add_flow(&ui, &root, pkg, cli.verbose)
        }
        Some(Commands::Remove { pkg }) => {
            let root = load_scope()?;
            lockflow::remove_flow(&ui, &root, pkg, cli.verbose)
        }
        Some(Commands::Vendor { dry_run }) => {
            let root = load_scope()?;
            lockflow::vendor_flow(&ui, &root, *dry_run, cli.verbose)
        }
        Some(Commands::Dev { name }) => {
            let root = load_scope()?;
            lockflow::dev_flow(&ui, &root, name, cli.verbose)
        }
        Some(Commands::Completions { shell }) => cmd_completions(shell),
        Some(Commands::Publish {
            registry,
            token,
            dry_run,
            package,
        }) => {
            let root = load_scope()?;
            for (pkg, key) in lockflow::active_patches(&root)? {
                ui.warn(&format!(
                    "patch override active for `{key}` in {pkg}; remove it before publishing"
                ));
            }
            lockflow::publish_flow(
                &ui,
                &root,
                package.clone(),
                registry.clone(),
                token.clone(),
                *dry_run,
                cli.verbose,
            )
        }
        Some(Commands::Login { registry, token }) => {
            lockflow::login_flow(&ui, registry.clone(), token.clone())
        }
        Some(Commands::Search { query, registry }) => {
            lockflow::search_flow(&ui, query.clone(), registry.clone(), cli.verbose)
        }
        Some(Commands::Install {
            package,
            bin,
            release,
            jobs,
            features,
            all_features,
            no_default_features,
            offline,
            locked,
            frozen,
        }) => cmd_install(
            &ui,
            InstallFlags {
                package: package.clone(),
                bin: bin.clone(),
                release: *release,
                jobs: *jobs,
                features: feature_flags(features, *all_features, *no_default_features),
                offline: *offline,
                locked: *locked,
                frozen: *frozen,
                verbose: cli.verbose,
                json: ui.json,
            },
        ),
        Some(Commands::Clean { package }) => cmd_clean(&ui, package.clone()),
        Some(Commands::Toolchains) => cmd_toolchains(&ui),
        Some(Commands::Tree) => {
            let root = load_scope()?;
            cmd_tree(&ui, &root)
        }
        Some(Commands::Fmt { check }) => {
            let root = load_scope()?;
            cmd_fmt(&ui, &root, *check)
        }
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
    let name = match cmd {
        Commands::Check {} => "check",
        Commands::Doc => "doc",
        _ => "command",
    };
    format!("`rivet {name}` is not supported yet")
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

/// What `new`/`init` scaffolds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewKind {
    /// Binary (`src/main.cpp`).
    Bin,
    /// Static library (`src/lib.cpp` + `include/<name>.h`).
    Lib,
    /// Shared library (same layout, `lib.type = "shared"`).
    Shared,
}

impl NewKind {
    /// Resolve `--lib`/`--shared` (clap already rejects both at once, but
    /// the test seam builds `Commands` by hand, so check again here).
    fn resolve(lib: bool, shared: bool) -> Result<Self> {
        match (lib, shared) {
            (false, false) => Ok(Self::Bin),
            (true, false) => Ok(Self::Lib),
            (false, true) => Ok(Self::Shared),
            (true, true) => {
                bail!("`--lib` and `--shared` are mutually exclusive. Fix: pick one")
            }
        }
    }

    fn status_noun(self) -> &'static str {
        match self {
            Self::Bin => "binary package",
            Self::Lib => "library package",
            Self::Shared => "shared-library package",
        }
    }
}

/// Header-guard macro for a package name (`foo-bar` → `FOO_BAR_H`).
fn header_guard(name: &str) -> String {
    let mut guard: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    guard.push_str("_H");
    guard
}

fn lib_header_template(name: &str) -> String {
    let guard = header_guard(name);
    format!("#ifndef {guard}\n#define {guard}\n\nint answer();\n\n#endif\n")
}

fn lib_cpp_template(name: &str) -> String {
    format!("#include \"{name}.h\"\n\nint answer() {{ return 42; }}\n")
}

fn lib_manifest_template(name: &str, shared: bool) -> String {
    let lib = if shared {
        format!("\"name\": \"{name}\",\n    \"type\": \"shared\"")
    } else {
        format!("\"name\": \"{name}\"")
    };
    format!(
        r#"{{
  "$schema": "https://raw.githubusercontent.com/cfuncdoor/rivet/main/rivet.schema.json",
  "package": {{
    "name": "{name}",
    "version": "0.1.0",
    "standard": "c++17"
  }},
  "lib": {{
    {lib}
  }}
}}
"#
    )
}

fn manifest_template(name: &str) -> String {
    format!(
        r#"{{
  "$schema": "https://raw.githubusercontent.com/cfuncdoor/rivet/main/rivet.schema.json",
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

fn scaffold_package(ui: &Ui, dir: &Path, name: &str, kind: NewKind) -> Result<()> {
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
    let manifest_text = match kind {
        NewKind::Bin => manifest_template(name),
        NewKind::Lib => lib_manifest_template(name, false),
        NewKind::Shared => lib_manifest_template(name, true),
    };
    std::fs::write(&manifest_path, manifest_text)
        .with_context(|| format!("could not write {}", manifest_path.display()))?;
    match kind {
        NewKind::Bin => {
            let main_cpp = dir.join("src").join("main.cpp");
            if !main_cpp.exists() {
                std::fs::write(&main_cpp, MAIN_CPP_TEMPLATE)
                    .with_context(|| format!("could not write {}", main_cpp.display()))?;
            }
        }
        NewKind::Lib | NewKind::Shared => {
            std::fs::create_dir_all(dir.join("include"))
                .with_context(|| format!("could not create {}", dir.join("include").display()))?;
            let header = dir.join("include").join(format!("{name}.h"));
            if !header.exists() {
                std::fs::write(&header, lib_header_template(name))
                    .with_context(|| format!("could not write {}", header.display()))?;
            }
            let lib_cpp = dir.join("src").join("lib.cpp");
            if !lib_cpp.exists() {
                std::fs::write(&lib_cpp, lib_cpp_template(name))
                    .with_context(|| format!("could not write {}", lib_cpp.display()))?;
            }
        }
    }
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, GITIGNORE_TEMPLATE)?;
    }
    ui.status(
        "Created",
        &format!("{} `{name}` in {}", kind.status_noun(), dir.display()),
    );
    Ok(())
}

fn cmd_new(ui: &Ui, name: &str, kind: NewKind) -> Result<()> {
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
    scaffold_package(ui, &dir, name, kind)
}

fn cmd_init(ui: &Ui, name: Option<String>, kind: NewKind) -> Result<()> {
    let cwd = std::env::current_dir().context("could not read current directory")?;
    let default_name = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("hello")
        .to_string();
    let name = name.unwrap_or(default_name);
    scaffold_package(ui, &cwd, &name, kind)
}

// ---------------------------------------------------------------------------
// Commands: build / run / clean / toolchains
// ---------------------------------------------------------------------------

/// Build CLI `--features`/`--all-features`/`--no-default-features` into a
/// [`rivet_graph::FeatureFlags`].
fn feature_flags(
    features: &[String],
    all_features: bool,
    no_default_features: bool,
) -> rivet_graph::FeatureFlags {
    rivet_graph::FeatureFlags {
        features: features.to_vec(),
        all: all_features,
        no_default: no_default_features,
    }
}

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
    /// Feature selection.
    pub features: rivet_graph::FeatureFlags,
    /// `--target` triple (cross-compile, `native` default).
    pub target: Option<String>,
    /// Verbose logging.
    pub verbose: bool,
    /// JSON message format.
    pub json: bool,
    /// No network (cache/vendor only).
    pub offline: bool,
    /// Error if the lockfile would change.
    pub locked: bool,
    /// No network + error if the lockfile would change.
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
    /// Feature selection.
    pub features: rivet_graph::FeatureFlags,
    /// Cross-compile target triple.
    pub target: Option<String>,
    /// Arguments forwarded to the binary.
    pub args: Vec<OsString>,
    /// Verbose logging.
    pub verbose: bool,
    /// JSON message format.
    pub json: bool,
    /// No network (cache/vendor only).
    pub offline: bool,
    /// Error if the lockfile would change.
    pub locked: bool,
    /// No network + error if the lockfile would change.
    pub frozen: bool,
}

/// Flags for [`cmd_test`].
pub struct TestFlags {
    /// Use the `release` profile.
    pub release: bool,
    /// `-p` package selector (plus its path deps).
    pub package: Option<String>,
    /// Parallel compile jobs (`0` = auto).
    pub jobs: usize,
    /// Report all compile errors instead of stopping at the first.
    pub keep_going: bool,
    /// Feature selection.
    pub features: rivet_graph::FeatureFlags,
    /// Cross-compile target triple.
    pub target: Option<String>,
    /// Verbose logging.
    pub verbose: bool,
    /// JSON message format.
    pub json: bool,
    /// No network (cache/vendor only).
    pub offline: bool,
    /// Error if the lockfile would change.
    pub locked: bool,
    /// No network + error if the lockfile would change.
    pub frozen: bool,
}

/// Flags for [`cmd_compile_commands`].
pub struct CompileCommandsFlags {
    /// Use the `release` profile flags.
    pub release: bool,
    /// `-p` package selector (plus its path deps).
    pub package: Option<String>,
    /// Feature selection.
    pub features: rivet_graph::FeatureFlags,
    /// Cross-compile target triple.
    pub target: Option<String>,
    /// Verbose logging.
    pub verbose: bool,
    /// No network (cache/vendor only).
    pub offline: bool,
    /// Error if the lockfile would change.
    pub locked: bool,
    /// No network + error if the lockfile would change.
    pub frozen: bool,
}

/// Flags for [`cmd_install`].
pub struct InstallFlags {
    /// `-p` package selector.
    pub package: Option<String>,
    /// `--bin` binary selector.
    pub bin: Option<String>,
    /// Use the `release` profile.
    pub release: bool,
    /// Parallel compile jobs (`0` = auto).
    pub jobs: usize,
    /// Feature selection.
    pub features: rivet_graph::FeatureFlags,
    /// No network (cache/vendor only).
    pub offline: bool,
    /// Error if the lockfile would change.
    pub locked: bool,
    /// No network + error if the lockfile would change.
    pub frozen: bool,
    /// Verbose logging.
    pub verbose: bool,
    /// JSON message format.
    pub json: bool,
}

/// Build the scope, then copy every selected binary to `~/.rivet/bin`.
fn cmd_install(ui: &Ui, flags: InstallFlags) -> Result<()> {
    let built = cmd_build(
        ui,
        BuildFlags {
            release: flags.release,
            package: flags.package.clone(),
            bin: flags.bin.clone(),
            jobs: flags.jobs,
            keep_going: false,
            features: flags.features.clone(),
            target: None,
            verbose: flags.verbose,
            json: flags.json,
            offline: flags.offline,
            locked: flags.locked,
            frozen: flags.frozen,
        },
    )?;
    let root = rivet_cache::cache_root().map_err(|e| anyhow::anyhow!("{e}"))?;
    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir)
        .with_context(|| format!("could not create {}", bin_dir.display()))?;
    let mut bins: Vec<&PathBuf> = Vec::new();
    for b in &built {
        for target in &b.plan.bins {
            if let Some(want) = &flags.bin {
                if target.name != *want {
                    continue;
                }
            }
            bins.push(&target.output);
        }
    }
    if bins.is_empty() {
        bail!("no binaries selected. Fix: check `-p`/`--bin` selection");
    }
    for exe in bins {
        let name = exe
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("binary has no file name"))?;
        let dst = bin_dir.join(name);
        std::fs::copy(exe, &dst)
            .with_context(|| format!("could not copy {} to {}", exe.display(), dst.display()))?;
        ui.status("Installed", &format!("{}", dst.display()));
    }
    Ok(())
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

/// Resolve the toolchain for an optional `--target` request.
/// `None` (or a native triple) means normal detection; anything else goes
/// through clang `--target=` or fails actionably. The selection's triple is
/// recorded on the toolchain so fingerprints and driver flags follow.
fn select_toolchain(
    ui: &Ui,
    target: Option<&str>,
    verbose: bool,
) -> Result<rivet_toolchain::CxxToolchain> {
    use rivet_toolchain::{select_for_target, TargetTriple};
    let requested = target
        .map(TargetTriple::parse)
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // Native detections go through the on-disk toolchain cache (M8): the
    // entry is validated by driver existence + a live version probe.
    // Cross-target detection stays live (already cheap: no vcvars probe).
    if requested.as_ref().map(|t| t.is_native()).unwrap_or(true) {
        if let Ok(root) = rivet_cache::cache_root() {
            if let Some(mut cached) = rivet_toolchain::load_cached_toolchain(&root) {
                cached.target = requested;
                if verbose {
                    ui.verbose(&format!("toolchain: cached {}", cached.cxx.display()));
                }
                return Ok(cached);
            }
        }
    }
    let selection = select_for_target(requested.as_ref()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut toolchain = selection.toolchain;
    toolchain.target = requested;
    if !selection.is_cross {
        if let Ok(root) = rivet_cache::cache_root() {
            if let Err(e) = rivet_toolchain::store_cached_toolchain(&root, &toolchain) {
                if verbose {
                    ui.verbose(&format!("toolchain cache store failed: {e}"));
                }
            }
        }
    }
    Ok(toolchain)
}

/// Build every unit in scope, returning them with their plans.
/// Prints Cargo-like `Compiling`/`Fresh`/`Cached`/`Finished` lines.
fn cmd_build(ui: &Ui, flags: BuildFlags) -> Result<Vec<BuiltUnit>> {
    let root = load_scope()?;
    let profile = if flags.release { "release" } else { "dev" };
    let mut units = lockflow::resolve_build_units(
        ui,
        &root,
        profile,
        flags.package.as_deref(),
        &flags.features,
        &lockflow::LockOpts {
            offline: flags.offline,
            locked: flags.locked,
            frozen: flags.frozen,
        },
        flags.verbose,
    )?;
    if let Some(bin) = &flags.bin {
        units = restrict_to_bin(units, bin)?;
    }

    let detection = select_toolchain(ui, flags.target.as_deref(), flags.verbose)?;
    log_toolchain(ui, &detection);
    if let Some(t) = &detection.target {
        ui.verbose(&format!("target: {t}"));
    }
    let cache_root = rivet_cache::cache_root().map_err(|e| anyhow::anyhow!("{e}"))?;
    let build_opts = rivet_graph::BuildOptions {
        verbose: flags.verbose,
        jobs: flags.jobs,
        fail_fast: !flags.keep_going,
        cache_dir: Some(cache_root),
    };
    let started = std::time::Instant::now();
    let mut built = Vec::new();
    build_adapter_units(
        ui,
        &mut units,
        profile,
        &detection,
        flags.jobs,
        flags.verbose,
    )?;
    for unit in &units {
        if unit.adapter.is_some() {
            // Adapter outputs feed dependents through `adapter_output`;
            // they have no rivet targets of their own.
            built.push(BuiltUnit {
                unit: unit.clone(),
                plan: adapter_empty_plan(&unit.manifest, profile),
            });
            continue;
        }
        let plan = build_single_unit(ui, unit, &units, profile, &detection, &build_opts)?;
        built.push(BuiltUnit {
            unit: unit.clone(),
            plan,
        });
    }
    let secs = started.elapsed().as_secs_f64();
    ui.status("Finished", &format!("{profile} profile in {secs:.2}s"));
    Ok(built)
}

/// Run the adapter flow for every adapter unit in topo order (sync loop:
/// clone out, build, write back so dependents see `adapter_output`).
fn build_adapter_units(
    ui: &Ui,
    units: &mut [rivet_graph::workspace::Unit],
    profile: &str,
    toolchain: &rivet_toolchain::CxxToolchain,
    jobs: usize,
    verbose: bool,
) -> Result<()> {
    for unit in units.iter_mut() {
        if unit.adapter.is_some() {
            build_adapter_unit(ui, unit, profile, toolchain, jobs, verbose)?;
        }
    }
    Ok(())
}

/// Empty plan placeholder for adapter-built units (their outputs feed
/// dependents through `adapter_output`, never through rivet targets).
fn adapter_empty_plan(
    manifest: &rivet_manifest::Manifest,
    profile: &str,
) -> rivet_graph::PackagePlan {
    rivet_graph::PackagePlan {
        profile: profile.to_string(),
        bins: Vec::new(),
        lib: None,
        header_only: false,
        target_dir: manifest
            .dir
            .join("target")
            .join(rivet_graph::profile_dir(profile)),
    }
}

/// Build one foreign unit through its adapter (or probe it when `system`),
/// filling `unit.adapter_output` for dependents later in topo order.
/// Adapter spec/options live in `lockflow::adapter_fingerprint_paths`'s
/// sibling state file next to the unit's target dir.
fn build_adapter_unit(
    ui: &Ui,
    unit: &mut rivet_graph::workspace::Unit,
    profile: &str,
    toolchain: &rivet_toolchain::CxxToolchain,
    jobs: usize,
    verbose: bool,
) -> Result<()> {
    use rivet_adapters::{build_adapter, probe_installed, AdapterKind};
    let name = rivet_manifest::package_name(&unit.manifest).unwrap_or("<unknown>");
    let version = rivet_manifest::package_version(&unit.manifest).unwrap_or("0.0.0");
    let spec = unit
        .adapter
        .clone()
        .ok_or_else(|| anyhow::anyhow!("adapter unit `{name}` lost its spec (internal error)"))?;
    // Guardrail: adapter units must be foreign. Explicit `bin`/`lib`
    // declarations plus an `adapter` are contradictory (one entry point
    // would silently never build); lib *conventions* (`src/lib.cpp`,
    // leftover sources, `include/` headers) cannot distinguish foreign
    // sources, so the adapter wins over them.
    let bins = rivet_manifest::resolve_bins(&unit.manifest);
    if !bins.is_empty() || unit.manifest.lib.is_some() {
        bail!(
            "adapter dep `{name}` declares rivet targets (bin/lib) and an `adapter`. Fix: remove the `adapter` to build it as a rivet package, or drop `bin`/`lib` to build it as foreign"
        );
    }
    let base = unit_target_dir(unit, profile);
    std::fs::create_dir_all(&base)
        .with_context(|| format!("could not create {}", base.display()))?;
    let state_path = base.join("adapter-fingerprint.json");

    // System probes run every build (millisecond probes; the installed
    // library may change under us — no fingerprint gate).
    if spec.tool == "system"
        || !AdapterKind::from_tool(&spec.tool)
            .map(|k| k.builds_sources())
            .unwrap_or(false)
    {
        let probe_tool = if spec.tool.is_empty() {
            "system"
        } else {
            spec.tool.as_str()
        };
        let output = match AdapterKind::from_tool(probe_tool) {
            Ok(AdapterKind::PkgConfig) | Ok(AdapterKind::System) => {
                probe_installed(probe_tool, name).map_err(|e| anyhow::anyhow!("{e}"))?
            }
            _ => {
                // `cmake`/`meson`/`make` with no sources cannot probe.
                bail!(
                    "adapter dep `{name}` has no sources to build with `{}`. Fix: give it a `path`/`git` source or use a probing tool",
                    spec.tool
                );
            }
        };
        ui.status("Fresh", &format!("{name} v{version} (system probe)"));
        ui.verbose(&format!(
            "probed {name}: {} includes, {} libs",
            output.include_dirs.len(),
            output.libs.len()
        ));
        unit.adapter_output = Some(output);
        return Ok(());
    }

    // Source builds: fingerprint gate (source bytes + spec + toolchain).
    let tc_fp = toolchain.fingerprint(&rivet_graph::compile_options(
        &unit.manifest,
        profile,
        &[],
        &unit.features,
        &[],
    ));
    let fp_now = rivet_adapters::fingerprint(&unit.dir, &spec, &tc_fp)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(prev) = lockflow::read_adapter_state(&state_path)? {
        if prev.fingerprint == fp_now {
            ui.status("Fresh", &format!("{name} v{version} (adapter)"));
            unit.adapter_output = Some(prev.output);
            return Ok(());
        }
    }
    ui.status(
        "Compiling",
        &format!("{name} v{version} ({profile}, {})", spec.tool),
    );
    let build_type = if profile == "release" {
        "Release"
    } else {
        "Debug"
    };
    let build_dir = base.join("adapter-build");
    // `make` builds in-source: hand it a writable scratch copy (the real
    // sources may live in an immutable checkout).
    let source_dir = if spec.tool == "make" {
        let copy = base.join("adapter-src");
        if copy.is_dir() {
            std::fs::remove_dir_all(&copy)
                .with_context(|| format!("could not clear {}", copy.display()))?;
        }
        lockflow::copy_dir_filtered(&unit.dir, &copy, true)?;
        copy
    } else {
        unit.dir.clone()
    };
    let mut ctx = rivet_adapters::AdapterCtx::new(build_dir).with_build_type(build_type);
    if jobs > 0 {
        ctx = ctx.with_jobs(jobs);
    }
    ctx = ctx.with_toolchain(toolchain.clone());
    let output = build_adapter(&source_dir, &spec, &ctx).map_err(|e| anyhow::anyhow!("{e}"))?;
    if verbose {
        eprintln!(
            "       adapter {name}: {} includes, {} libs",
            output.include_dirs.len(),
            output.libs.len()
        );
    }
    lockflow::write_adapter_state(&state_path, &fp_now, &output)?;
    unit.adapter_output = Some(output);
    Ok(())
}

/// Target dir for a unit: locked-materialized units redirect under the
/// scope target dir (never into immutable checkouts/vendor copies).
fn unit_target_dir(unit: &rivet_graph::workspace::Unit, profile: &str) -> PathBuf {
    unit.target_dir_override
        .clone()
        .unwrap_or_else(|| rivet_graph::default_target_dir(&unit.manifest.dir, profile))
}

/// Build one unit (normal lib+bins plan), printing `Fresh` or `Compiling`
/// plus `-v` detail. Returns the plan for callers that extend it (tests).
fn build_single_unit(
    ui: &Ui,
    unit: &rivet_graph::workspace::Unit,
    units: &[rivet_graph::workspace::Unit],
    profile: &str,
    detection: &rivet_toolchain::CxxToolchain,
    build_opts: &rivet_graph::BuildOptions,
) -> Result<rivet_graph::PackagePlan> {
    let name = rivet_manifest::package_name(&unit.manifest).unwrap_or("<unknown>");
    let version = rivet_manifest::package_version(&unit.manifest).unwrap_or("0.0.0");
    let target_dir = unit_target_dir(unit, profile);
    let plan = rivet_graph::plan_with_target(&unit.manifest, profile, detection, &target_dir)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let inputs = rivet_graph::workspace::unit_inputs(unit, units, profile, detection)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if rivet_graph::is_fresh_unit(&unit.manifest, &plan, detection, &inputs, &unit.features)
        .map_err(|e| anyhow::anyhow!("{e}"))?
    {
        ui.status("Fresh", &format!("{name} v{version}"));
        // `build_unit` never runs for fresh units, so refresh staged
        // runtime files here (a DLL can change under an identical import
        // library without tripping freshness).
        rivet_graph::ensure_staged_outputs(&unit.manifest, &plan, &inputs)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    } else {
        ui.status("Compiling", &format!("{name} v{version} ({profile})"));
        let summary = rivet_graph::build_unit(
            &unit.manifest,
            &plan,
            detection,
            &inputs,
            build_opts,
            &unit.features,
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        if summary.cached {
            ui.status("Cached", &format!("{name} v{version}"));
        }
        report_build_summary(ui, &summary);
    }
    Ok(plan)
}

/// Replay a build summary's `-v` log and recompiled/relinked notes.
fn report_build_summary(ui: &Ui, summary: &rivet_graph::UnitSummary) {
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

/// Log the detected toolchain at `-v`.
fn log_toolchain(ui: &Ui, detection: &rivet_toolchain::CxxToolchain) {
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

fn cmd_test(ui: &Ui, flags: TestFlags) -> Result<()> {
    let root = load_scope()?;
    let profile = if flags.release { "release" } else { "dev" };
    let units = lockflow::resolve_build_units(
        ui,
        &root,
        profile,
        flags.package.as_deref(),
        &flags.features,
        &lockflow::LockOpts {
            offline: flags.offline,
            locked: flags.locked,
            frozen: flags.frozen,
        },
        flags.verbose,
    )?;

    let detection = select_toolchain(ui, flags.target.as_deref(), flags.verbose)?;
    log_toolchain(ui, &detection);
    if let Some(t) = &detection.target {
        ui.verbose(&format!("target: {t}"));
    }
    let cache_root = rivet_cache::cache_root().map_err(|e| anyhow::anyhow!("{e}"))?;
    let build_opts = rivet_graph::BuildOptions {
        verbose: flags.verbose,
        jobs: flags.jobs,
        fail_fast: !flags.keep_going,
        cache_dir: Some(cache_root),
    };
    let started = std::time::Instant::now();
    let mut passed: usize = 0;
    let mut failed: usize = 0;
    let mut failures: Vec<String> = Vec::new();
    let mut units = units;
    build_adapter_units(
        ui,
        &mut units,
        profile,
        &detection,
        flags.jobs,
        flags.verbose,
    )?;
    for unit in &units {
        if unit.adapter.is_some() {
            // No rivet tests inside foreign sources.
            continue;
        }
        let name = rivet_manifest::package_name(&unit.manifest).unwrap_or("<unknown>");
        let version = rivet_manifest::package_version(&unit.manifest).unwrap_or("0.0.0");
        // 1. Normal build: lib + bins first.
        let plan = build_single_unit(ui, unit, &units, profile, &detection, &build_opts)?;
        // 2. Then tests (synthesized targets through the same incremental
        //    state), then run each binary sequentially.
        let inputs = rivet_graph::workspace::unit_inputs(unit, &units, profile, &detection)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let (tests, summary) = rivet_graph::test::build_tests(
            &unit.manifest,
            &plan,
            &detection,
            &inputs,
            &build_opts,
            &unit.features,
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        if !tests.is_empty() {
            let did_work = summary
                .recompiled
                .iter()
                .any(|(tag, _)| tag.starts_with("test-"));
            if did_work {
                ui.status("Compiling", &format!("{name} v{version} (tests)"));
            } else {
                ui.status("Fresh", &format!("{name} v{version} (tests)"));
            }
            report_build_summary(ui, &summary);
            for target in &tests {
                let run =
                    rivet_graph::test::run_test_binary(&target.output, &target.name, &unit.dir)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                if run.passed {
                    ui.status("test", &format!("{} ... ok", run.name));
                    passed += 1;
                } else {
                    ui.status("test", &format!("{} ... FAILED", run.name));
                    let out = String::from_utf8_lossy(&run.stdout);
                    let err = String::from_utf8_lossy(&run.stderr);
                    if !out.is_empty() {
                        println!("{out}");
                    }
                    if !err.is_empty() {
                        eprintln!("{err}");
                    }
                    failed += 1;
                    failures.push(run.name);
                }
            }
        }
    }
    let secs = started.elapsed().as_secs_f64();
    ui.status("Finished", &format!("{profile} profile in {secs:.2}s"));
    println!("test result: ok. {passed} passed; {failed} failed");
    if failed > 0 {
        failures.sort();
        bail!(
            "tests failed: {} ({} passed, {} failed)",
            failures.join(", "),
            passed,
            failed
        );
    }
    Ok(())
}

fn cmd_compile_commands(ui: &Ui, flags: CompileCommandsFlags) -> Result<()> {
    let root = load_scope()?;
    let scope_dir = root.dir.clone();
    let profile = if flags.release { "release" } else { "dev" };
    let units = lockflow::resolve_build_units(
        ui,
        &root,
        profile,
        flags.package.as_deref(),
        &flags.features,
        &lockflow::LockOpts {
            offline: flags.offline,
            locked: flags.locked,
            frozen: flags.frozen,
        },
        flags.verbose,
    )?;

    // Flag mapping needs a toolchain, but nothing is compiled.
    let detection = select_toolchain(ui, flags.target.as_deref(), flags.verbose)?;
    log_toolchain(ui, &detection);
    let mut units = units;
    build_adapter_units(ui, &mut units, profile, &detection, 0, flags.verbose)?;
    let mut merged: Vec<rivet_graph::compile_commands::CompileEntry> = Vec::new();
    for unit in &units {
        if unit.adapter.is_some() {
            // Foreign sources are not rivet translation units.
            continue;
        }
        let target_dir = unit_target_dir(unit, profile);
        let plan = rivet_graph::plan_with_target(&unit.manifest, profile, &detection, &target_dir)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let inputs = rivet_graph::workspace::unit_inputs(unit, &units, profile, &detection)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut entries = rivet_graph::compile_commands::compile_commands_for(
            &unit.manifest,
            &plan,
            &detection,
            &inputs,
            &unit.features,
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        entries.sort_by(|a, b| a.file.cmp(&b.file));
        let path = plan.target_dir.join("compile_commands.json");
        rivet_graph::compile_commands::write_compile_commands(&path, &entries)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        ui.status("Generated", &format!("{}", path.display()));
        merged.extend(entries);
    }
    if units.len() > 1 {
        merged.sort_by(|a, b| a.file.cmp(&b.file));
        let path = scope_dir
            .join("target")
            .join(rivet_graph::profile_dir(profile))
            .join("compile_commands.json");
        rivet_graph::compile_commands::write_compile_commands(&path, &merged)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        ui.status("Generated", &format!("{}", path.display()));
    }
    Ok(())
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
            features: flags.features.clone(),
            target: flags.target.clone(),
            verbose: flags.verbose,
            json: flags.json,
            offline: flags.offline,
            locked: flags.locked,
            frozen: flags.frozen,
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

/// Re-resolve branch deps and rewrite the lockfile.
///
/// Resolution covers every optional dependency (`--all-features`
/// semantics) so later `--features` builds never need to extend the lock.
fn cmd_update(ui: &Ui, opts: lockflow::UpdateOpts, verbose: bool) -> Result<()> {
    let root = load_scope()?;
    let flags = rivet_graph::FeatureFlags {
        features: Vec::new(),
        all: true,
        no_default: false,
    };
    lockflow::update_flow(ui, &root, &flags, &opts, verbose)
}

/// Git cache management.
fn cmd_git(ui: &Ui, command: &GitCommands, verbose: bool) -> Result<()> {
    let root = load_scope()?;
    match command {
        GitCommands::List => lockflow::git_list_flow(ui, &root, verbose),
        GitCommands::Prune { dry_run } => lockflow::git_prune_flow(ui, &root, *dry_run, verbose),
        GitCommands::Fetch => lockflow::git_fetch_flow(ui, &root, verbose),
    }
}

/// Print shell completions to stdout.
fn cmd_completions(shell: &str) -> Result<()> {
    use clap::CommandFactory;
    use clap_complete::{generate, shells};
    let mut cmd = Cli::command();
    match shell.to_ascii_lowercase().as_str() {
        "bash" => generate(shells::Bash, &mut cmd, "rivet", &mut std::io::stdout()),
        "zsh" => generate(shells::Zsh, &mut cmd, "rivet", &mut std::io::stdout()),
        "fish" => generate(shells::Fish, &mut cmd, "rivet", &mut std::io::stdout()),
        "powershell" => generate(
            shells::PowerShell,
            &mut cmd,
            "rivet",
            &mut std::io::stdout(),
        ),
        "elvish" => generate(shells::Elvish, &mut cmd, "rivet", &mut std::io::stdout()),
        other => {
            bail!("unknown shell `{other}`. Fix: use one of bash, zsh, fish, powershell, elvish")
        }
    }
    Ok(())
}

/// Print the scope's dependency tree (manifest-declared deps, with locked
/// versions when `rivet.lock.json` exists). Path-only scopes need no lock.
fn cmd_tree(ui: &Ui, root: &rivet_manifest::Manifest) -> Result<()> {
    let seeds = lockflow::scope_seeds(root)?;
    let locked: std::collections::BTreeMap<String, String> =
        lockflow::lock_versions(&root.dir).unwrap_or_default();
    for seed in &seeds {
        let name = rivet_manifest::package_name(seed).unwrap_or("<root>");
        let version = rivet_manifest::package_version(seed).unwrap_or("0.0.0");
        ui.status("Tree", &format!("{name} v{version}"));
        let mut deps: Vec<(&String, &rivet_manifest::Dependency)> =
            seed.dependencies.iter().collect();
        deps.sort_by(|a, b| a.0.cmp(b.0));
        for (key, dep) in deps {
            let detail = lockflow::describe_dep(dep);
            let pinned = locked
                .get(key.as_str())
                .map(|v| format!(" (locked v{v})"))
                .unwrap_or_default();
            println!("        {key} {detail}{pinned}");
        }
    }
    Ok(())
}

/// Format `src/`, `include/`, and `tests/` C++ sources via clang-format.
/// Delegates entirely (never reformats itself); missing binary is an
/// actionable error, `--check` only reports.
fn cmd_fmt(ui: &Ui, root: &rivet_manifest::Manifest, check: bool) -> Result<()> {
    let seeds = lockflow::scope_seeds(root)?;
    let clang_format = which::which("clang-format").map_err(|_| {
        anyhow::anyhow!(
            "clang-format not found in PATH. Fix: install LLVM/Clang (https://llvm.org) or `pip install clang-format`"
        )
    })?;
    // NOTE: `which` is intentionally used only here (user-facing tool
    // lookup), not for compilers (rivet-toolchain owns that).
    let mut files = Vec::new();
    for seed in &seeds {
        for dir in ["src", "include", "tests"] {
            collect_cpp(&seed.dir.join(dir), &mut files);
        }
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        ui.status(
            "Fmt",
            "nothing to format (no C++ sources under src/, include/, tests/)",
        );
        return Ok(());
    }
    if check {
        let out = std::process::Command::new(&clang_format)
            .arg("--dry-run")
            .arg("--Werror")
            .args(&files)
            .output()
            .with_context(|| "could not execute clang-format")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            eprint!("{stderr}");
            bail!("{} file(s) need formatting. Fix: run `rivet fmt`", 0);
        }
        ui.status("Fmt", &format!("{} file(s) clean", files.len()));
        return Ok(());
    }
    let status = std::process::Command::new(&clang_format)
        .arg("-i")
        .args(&files)
        .status()
        .with_context(|| "could not execute clang-format")?;
    if !status.success() {
        bail!("clang-format failed with {status}");
    }
    ui.status("Fmt", &format!("formatted {} file(s)", files.len()));
    Ok(())
}

/// Collect C++ source/header paths under `dir` (recursive, best-effort).
fn collect_cpp(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_cpp(&path, out);
        } else if rivet_manifest::is_cpp_source(&path) || rivet_manifest::is_header(&path) {
            out.push(path);
        }
    }
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
    fn new_kind_resolution() {
        assert_eq!(NewKind::resolve(false, false).unwrap(), NewKind::Bin);
        assert_eq!(NewKind::resolve(true, false).unwrap(), NewKind::Lib);
        assert_eq!(NewKind::resolve(false, true).unwrap(), NewKind::Shared);
        assert!(NewKind::resolve(true, true).is_err());
    }

    #[test]
    fn lib_templates_parse_and_guard() {
        assert_eq!(header_guard("foo-bar"), "FOO_BAR_H");
        assert_eq!(header_guard("a1"), "A1_H");
        for shared in [false, true] {
            let text = lib_manifest_template("mylib", shared);
            let m = rivet_manifest::parse_str(&text).unwrap();
            assert_eq!(rivet_manifest::package_name(&m), Some("mylib"));
            let lib = m.lib.expect("lib section");
            assert_eq!(lib.name.as_deref(), Some("mylib"));
            assert_eq!(
                lib.lib_type.as_str(),
                if shared { "shared" } else { "static" }
            );
        }
        assert!(lib_cpp_template("mylib").contains("#include \"mylib.h\""));
        assert!(lib_header_template("mylib").contains("int answer();"));
    }

    #[test]
    fn scope_root_stops_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        // No rivet.json anywhere above: scope root is the start dir itself.
        assert_eq!(find_scope_root(dir.path()), dir.path().to_path_buf());
    }

    #[test]
    fn unimplemented_commands_say_so() {
        let msg = unimplemented_message(&Commands::Check {});
        assert!(msg.contains("not supported"), "got: {msg}");
        // Implemented commands never reach the stub message.
        let msg = unimplemented_message(&Commands::Tree);
        assert!(msg.contains("not supported"), "got: {msg}");
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
