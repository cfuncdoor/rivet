//! Git dependency subsystem: shell-out to the system `git` binary only.
//!
//! Layout under `<cache>/git/` (see AGENTS.md):
//!
//! ```text
//! <cache>/git/
//!   empty-hooks/                       # empty dir used for `-c core.hooksPath=…`
//!   db/<name>-<urlhash>/               # one bare mirror per URL
//!     rivet-url.txt                    # the URL this mirror was cloned from
//!   checkouts/<name>-<urlhash>/<short-sha>/  # immutable trees, HEAD == full sha
//! ```
//!
//! * Only the system `git` binary is used (`std::process::Command`; never
//!   git2/gitoxide, per AGENTS.md). Honors SSH/credential helpers and
//!   `~/.gitconfig`. Every invocation goes through [`GitCache::run_git`],
//!   which sets `GIT_TERMINAL_PROMPT=0` (never prompts mid-build) and
//!   `-c core.hooksPath=<empty dir>` (hooks disabled).
//! * Allowed URL schemes: `https://`, `ssh://`, `file://`, and scp-like
//!   `git@host:path` SSH. Everything else is rejected (see [`check_url`]).
//!   Submodule URLs are gated the same way before any
//!   `submodule update --init` runs.
//! * Shallow/partial strategy: fetches first try `--filter=blob:none`
//!   (partial clone, exact ref only), then fall back to a full fetch of the
//!   same ref, then to a full `fetch origin`. Servers without filter support
//!   (notably `file://` test mirrors and old gits) fall through transparently.
//! * Same-URL-different-refs rule (see resolver crate docs too):
//!   one URL = one logical source. If two packages request the same URL with
//!   different refs, resolution must unify them to a single SHA; if they point
//!   at different SHAs that is an error spelling both refs. The checkout
//!   layout already keys trees by SHA so divergent SHAs *could* coexist, but
//!   building the same repo twice at different commits is a typo magnet, so
//!   rivet refuses it (Cargo-like strictness).
//!
//! [`GitCache`] is the entry point. `verbose(true)` makes every git command
//! print `+ git …` to stderr (the CLI wires `-v` through to this).

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A moved tag: the tag now points somewhere other than the locked SHA.
///
/// Tags must be immutable pins. If the remote moved a tag, resolution refuses
/// to silently follow it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedTag {
    /// Git URL the tag lives in.
    pub url: String,
    /// Tag name that moved.
    pub tag: String,
    /// SHA recorded in the lockfile.
    pub locked_sha: String,
    /// SHA the tag points at now.
    pub current_sha: String,
}

impl std::fmt::Display for MovedTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tag `{}` in `{}` moved (locked to {}, now points at {})",
            self.tag, self.url, self.locked_sha, self.current_sha
        )
    }
}

/// Git subsystem errors. Every variant is actionable (what/where/fix).
#[derive(Debug, Error)]
pub enum Error {
    /// System `git` binary is missing.
    #[error(
        "system `git` binary not found in PATH; install git from https://git-scm.com and retry"
    )]
    GitNotFound,
    /// URL uses a scheme rivet refuses to fetch.
    #[error("unsupported git URL `{url}`: {reason}. Hint: use https://, ssh://, git@ (scp-like ssh), or file://")]
    BadUrl {
        /// The offending URL.
        url: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A submodule of a git dep points at a disallowed URL.
    #[error("git dep `{url}`@{sha}: submodule `{name}` has unsupported URL `{sub_url}`. Hint: submodules must use https://, ssh://, git@, or file://; fix .gitmodules upstream or set submodules=false")]
    BadSubmoduleUrl {
        /// Parent dep URL.
        url: String,
        /// Resolved SHA.
        sha: String,
        /// Submodule name/path.
        name: String,
        /// Offending submodule URL.
        sub_url: String,
    },
    /// A `git` invocation failed.
    #[error("git command failed: {cmd}\nexit: {status}\nstderr: {stderr}\nHint: {hint}")]
    GitFailed {
        /// Full command line (for `-v` logs and bug reports).
        cmd: String,
        /// Exit status / signal description.
        status: String,
        /// Stderr (truncated).
        stderr: String,
        /// What to try next.
        hint: String,
    },
    /// Expected a full commit SHA but got something else.
    #[error("invalid git SHA `{sha}` for `{url}`. Hint: SHAs are 40 lowercase hex characters (got {len} chars); re-resolve the ref instead of hand-editing")]
    InvalidSha {
        /// Offending value.
        sha: String,
        /// URL context.
        url: String,
        /// Char length, for the hint.
        len: usize,
    },
    /// No such ref in the (possibly offline) mirror.
    #[error("could not resolve {what} in `{url}`: {detail}. Hint: {hint}")]
    Unresolvable {
        /// e.g. `tag "v1"`.
        what: String,
        /// URL context.
        url: String,
        /// Underlying detail.
        detail: String,
        /// Actionable next step.
        hint: String,
    },
    /// Network was needed but offline mode forbids it.
    #[error("offline: {action} needs network but --offline was given ({where_}). Hint: run once online to populate the mirror, then retry offline")]
    Offline {
        /// What was attempted.
        action: String,
        /// Where (URL or path).
        where_: String,
    },
    /// Checkout tree hash does not match the locked tree hash.
    #[error("tree hash mismatch for `{url}`@{sha}: expected tree {expected}, got {actual}. Hint: the remote history was rewritten; run `rivet update` online or verify the lockfile was not hand-edited")]
    TreeMismatch {
        /// URL context.
        url: String,
        /// Commit SHA.
        sha: String,
        /// Expected tree hash.
        expected: String,
        /// Actual tree hash.
        actual: String,
    },
    /// Path traversal / symlink escape detected.
    #[error("unsafe path in checkout `{root}`: `{path}` ({reason}). Hint: the upstream repo contains a traversal or escaping symlink; refuse to build it")]
    UnsafePath {
        /// Checkout root.
        root: String,
        /// Offending path.
        path: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A requested subdir is invalid or missing.
    #[error("bad `subdir` `{subdir}` for `{url}`: {reason}. Hint: use a relative path without `..` that exists in the checkout")]
    BadSubdir {
        /// URL context.
        url: String,
        /// Requested subdir.
        subdir: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A tag moved between the lockfile and the remote.
    #[error("moved tag `{0}`: re-pin with `rivet update` after auditing the upstream change")]
    MovedTag(MovedTag),
    /// Filesystem failure with context.
    #[error("{action} `{path}`: {message}")]
    Io {
        /// What was attempted.
        action: String,
        /// Path involved.
        path: String,
        /// OS message + fix hint.
        message: String,
    },
}

impl Error {
    fn io(action: &str, path: &Path, err: std::io::Error) -> Self {
        Self::Io {
            action: action.to_string(),
            path: path.display().to_string(),
            message: format!("{err}. Hint: check permissions and disk space"),
        }
    }
}

// ---------------------------------------------------------------------------
// URL gate
// ---------------------------------------------------------------------------

/// `true` when `url` uses an allowed scheme.
///
/// Allowed: `https://`, `ssh://`, `file://`, and scp-like `git@host:path`
/// SSH (no `://`, contains both `@` and `:` after the `@`).
pub fn is_allowed_url(url: &str) -> bool {
    if url.starts_with("https://") || url.starts_with("ssh://") || url.starts_with("file://") {
        return true;
    }
    // scp-like: git@github.com:org/repo.git (also plain host:path with git@).
    if let Some(rest) = url.strip_prefix("git@") {
        return rest.contains(':') && !rest.contains("://");
    }
    false
}

/// Reject URLs outside the allowed schemes with an actionable error.
///
/// The same gate applies to submodule URLs.
pub fn check_url(url: &str) -> Result<(), Error> {
    if is_allowed_url(url) {
        return Ok(());
    }
    let reason = if url.contains("://") {
        let scheme = url.split("://").next().unwrap_or("?");
        format!("scheme `{scheme}://` is not allowed")
    } else if url.contains('@') {
        "scp-like SSH URLs must look like `git@host:org/repo.git`".to_string()
    } else {
        "URL must start with https://, ssh://, file://, or git@".to_string()
    };
    Err(Error::BadUrl {
        url: url.to_string(),
        reason,
    })
}

// ---------------------------------------------------------------------------
// Layout helpers
// ---------------------------------------------------------------------------

/// Deterministic 64-bit FNV-1a hash, hex-encoded (16 chars).
///
/// Uses FNV instead of a crypto hash so `rivet-git` stays dependency-free
/// (AGENTS.md forbids new non-trivial deps without justification; a cache-dir
/// suffix is not worth `sha2`). Collisions across distinct URLs in one cache
/// are astronomically unlikely for this use, and the full URL is recorded in
/// `db/<dir>/rivet-url.txt` so a collision would surface as a URL mismatch
/// rather than silent confusion.
pub fn url_hash(url: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in url.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Make `name` filesystem-safe for the `<name>-<hash>` layout.
pub fn sanitize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        out.push_str("dep");
    }
    if out.len() > 64 {
        out.truncate(64);
    }
    out
}

/// `<git_root>/db/<name>-<urlhash>/` bare mirror path.
pub fn db_dir(git_root: &Path, name: &str, url: &str) -> PathBuf {
    git_root
        .join("db")
        .join(format!("{}-{}", sanitize_name(name), url_hash(url)))
}

/// `<git_root>/checkouts/<name>-<urlhash>/` base path.
pub fn checkout_base_dir(git_root: &Path, name: &str, url: &str) -> PathBuf {
    git_root
        .join("checkouts")
        .join(format!("{}-{}", sanitize_name(name), url_hash(url)))
}

/// First 12 hex chars (Cargo-style short SHA for directory names).
pub fn short_sha(sha: &str) -> &str {
    if sha.len() > 12 {
        &sha[..12]
    } else {
        sha
    }
}

/// `<git_root>/checkouts/<name>-<urlhash>/<short-sha>/` immutable tree path.
pub fn checkout_dir(git_root: &Path, name: &str, url: &str, sha: &str) -> PathBuf {
    checkout_base_dir(git_root, name, url).join(short_sha(sha))
}

/// Validate a full commit SHA (40 hex chars; also accepts 64 for SHA-256 repos).
fn validate_sha(url: &str, sha: &str) -> Result<(), Error> {
    let ok_len = sha.len() == 40 || sha.len() == 64;
    let ok_hex = sha.bytes().all(|b| b.is_ascii_hexdigit());
    if ok_len && ok_hex {
        return Ok(());
    }
    Err(Error::InvalidSha {
        sha: sha.to_string(),
        url: url.to_string(),
        len: sha.len(),
    })
}

/// Validate a `subdir` request: relative, no `..`, no absolute/drive prefix.
fn validate_subdir(url: &str, subdir: &str) -> Result<(), Error> {
    if subdir.is_empty() {
        return Err(Error::BadSubdir {
            url: url.to_string(),
            subdir: subdir.to_string(),
            reason: "subdir is empty".to_string(),
        });
    }
    let p = Path::new(subdir);
    if p.is_absolute() {
        return Err(Error::BadSubdir {
            url: url.to_string(),
            subdir: subdir.to_string(),
            reason: "subdir must be relative".to_string(),
        });
    }
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                return Err(Error::BadSubdir {
                    url: url.to_string(),
                    subdir: subdir.to_string(),
                    reason: "subdir must not contain `..`".to_string(),
                });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::BadSubdir {
                    url: url.to_string(),
                    subdir: subdir.to_string(),
                    reason: "subdir must not be absolute".to_string(),
                });
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    // On Unix, `Path` does not split `\`, so check both separators plus
    // Windows drive prefixes explicitly (repos are shared across OSes).
    for part in subdir.split(['/', '\\']) {
        if part == ".." {
            return Err(Error::BadSubdir {
                url: url.to_string(),
                subdir: subdir.to_string(),
                reason: "subdir must not contain `..`".to_string(),
            });
        }
    }
    if subdir.len() >= 2
        && subdir.as_bytes()[1] == b':'
        && subdir.as_bytes()[0].is_ascii_alphabetic()
    {
        return Err(Error::BadSubdir {
            url: url.to_string(),
            subdir: subdir.to_string(),
            reason: "subdir must not have a drive prefix".to_string(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// GitRef
// ---------------------------------------------------------------------------

/// Which ref of a git URL a dependency pins.
///
/// `Tag` and `Branch` are floating (re-resolved on every `update`);
/// `Rev` is an exact SHA pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRef {
    /// Annotated or lightweight tag name (peeled to the commit).
    Tag(String),
    /// Branch name, or `"HEAD"` for the remote default branch.
    Branch(String),
    /// Full commit SHA.
    Rev(String),
}

impl GitRef {
    /// Build from manifest `tag`/`branch`/`rev` fields.
    ///
    /// Exactly one must be `Some`; `None`/`None`/`None` means the remote
    /// default branch (`Branch("HEAD")`).
    pub fn from_parts(
        tag: Option<&str>,
        branch: Option<&str>,
        rev: Option<&str>,
    ) -> Result<Self, Error> {
        let count = [tag, branch, rev].iter().filter(|v| v.is_some()).count();
        if count > 1 {
            return Err(Error::Unresolvable {
                what: "git ref".to_string(),
                url: String::new(),
                detail: "`tag`, `branch` and `rev` are mutually exclusive".to_string(),
                hint: "specify exactly one; prefer `tag` for releases, `rev` for pins".to_string(),
            });
        }
        if let Some(t) = tag {
            return Ok(Self::Tag(t.to_string()));
        }
        if let Some(b) = branch {
            return Ok(Self::Branch(b.to_string()));
        }
        if let Some(r) = rev {
            return Ok(Self::Rev(r.to_string()));
        }
        Ok(Self::Branch("HEAD".to_string()))
    }

    /// Short description for error messages (`tag "v1"`).
    pub fn describe(&self) -> String {
        match self {
            Self::Tag(t) => format!("tag \"{t}\""),
            Self::Branch(b) => format!("branch \"{b}\""),
            Self::Rev(r) => format!("rev \"{r}\""),
        }
    }

    /// `(kind, value)` pair for lockfile `ref` objects.
    pub fn kind_value(&self) -> (&'static str, &str) {
        match self {
            Self::Tag(v) => ("tag", v),
            Self::Branch(v) => ("branch", v),
            Self::Rev(v) => ("rev", v),
        }
    }
}

// ---------------------------------------------------------------------------
// Checkout options + cache handle
// ---------------------------------------------------------------------------

/// Options for [`GitCache::ensure_checkout`].
#[derive(Debug, Clone, Default)]
pub struct CheckoutOpts {
    /// Subdirectory inside the repo holding the package (as written).
    pub subdir: Option<String>,
    /// Run `submodule update --init` after checkout (URLs gated first).
    pub submodules: bool,
    /// Expected `sha^{tree}` hash; mismatch is an error (lock verification).
    pub expected_tree: Option<String>,
}

/// Cached-git handle rooted at `<cache>/git`.
///
/// Obtain the cache root from `RIVET_HOME` (tests/CI) or `~/.rivet`
/// (same rule as `rivet-cache::cache_root`, mirrored here so this crate
/// stays dependency-free), then `GitCache::new(&cache_root)`.
#[derive(Debug, Clone)]
pub struct GitCache {
    git_root: PathBuf,
    empty_hooks: PathBuf,
    verbose: bool,
    offline: bool,
}

impl GitCache {
    /// Create a handle from the global cache root (`~/.rivet` or `RIVET_HOME`).
    ///
    /// The `git/` subdirectory (plus `empty-hooks/`) is created lazily on
    /// first use so constructing the handle never touches the filesystem.
    pub fn new(cache_root: &Path) -> Self {
        let git_root = cache_root.join("git");
        let empty_hooks = git_root.join("empty-hooks");
        Self {
            git_root,
            empty_hooks,
            verbose: false,
            offline: false,
        }
    }

    /// Create a handle directly from a `<cache>/git` directory (tests).
    pub fn from_git_root(git_root: PathBuf) -> Self {
        let empty_hooks = git_root.join("empty-hooks");
        Self {
            git_root,
            empty_hooks,
            verbose: false,
            offline: false,
        }
    }

    /// Enable/disable `+ git …` stderr logging (CLI `-v`).
    pub fn with_verbose(mut self, verbose: bool) -> Self {
        self.verbose = verbose;
        self
    }

    /// Enable/disable offline mode (no network; mirrors only).
    pub fn with_offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    /// The `<cache>/git` root.
    pub fn git_root(&self) -> &Path {
        &self.git_root
    }

    /// Mirror path for `(name, url)`.
    pub fn mirror_path(&self, name: &str, url: &str) -> PathBuf {
        db_dir(&self.git_root, name, url)
    }

    /// Checkout path for `(name, url, sha)` (tree root, before subdir).
    pub fn checkout_path(&self, name: &str, url: &str, sha: &str) -> PathBuf {
        checkout_dir(&self.git_root, name, url, sha)
    }

    fn ensure_empty_hooks(&self) -> Result<(), Error> {
        if self.empty_hooks.is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.empty_hooks)
            .map_err(|e| Error::io("create empty hooks dir", &self.empty_hooks, e))?;
        Ok(())
    }

    /// Run one `git` invocation; log the full command when verbose.
    ///
    /// All git traffic in this crate flows through here so `-v`, hooks
    /// disabling, and `GIT_TERMINAL_PROMPT=0` apply uniformly. Returns
    /// trimmed stdout.
    pub fn run_git(&self, args: &[&str], cwd: Option<&Path>) -> Result<String, Error> {
        self.ensure_empty_hooks()?;
        require_git()?;
        let mut cmd = std::process::Command::new("git");
        // Hooks disabled for every invocation (fetch/checkout/clone/submodule).
        let hooks_arg: OsString = {
            let mut s = OsString::from("core.hooksPath=");
            s.push(&self.empty_hooks);
            s
        };
        cmd.arg("-c");
        cmd.arg(hooks_arg);
        // Allow `file://` submodule transport (hermetic tests + local mirrors).
        // Network submodules are still gated by `check_url` before we init them.
        cmd.arg("-c");
        cmd.arg("protocol.file.allow=always");
        for a in args {
            cmd.arg(a);
        }
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        if self.verbose {
            let mut line = String::from("+ git");
            for a in args {
                line.push(' ');
                line.push_str(a);
            }
            if let Some(dir) = cwd {
                line.push_str("  # in ");
                line.push_str(&dir.display().to_string());
            }
            eprintln!("{line}");
        }
        let output = cmd.output().map_err(|e| Error::Io {
            action: "spawn `git`".to_string(),
            path: "git".to_string(),
            message: format!("{e}. Hint: install git from https://git-scm.com"),
        })?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = truncate(&stderr, 2000);
            let mut cmdline = String::from("git");
            for a in args {
                cmdline.push(' ');
                cmdline.push_str(a);
            }
            Err(Error::GitFailed {
                cmd: cmdline,
                status: format!("{}", output.status),
                stderr,
                hint: "run with -v to see the full command; check the URL, ref, and network access"
                    .to_string(),
            })
        }
    }

    fn run_git_in_db(&self, db: &Path, args: &[&str]) -> Result<String, Error> {
        let db_str = db.to_string_lossy().to_string();
        let mut full: Vec<&str> = vec!["--git-dir", &db_str];
        full.extend_from_slice(args);
        self.run_git(&full, None)
    }

    /// Ensure the bare mirror exists; returns its path.
    ///
    /// Creates it with `git clone --bare` on first use (shallow/partial
    /// first, full fallback). Offline with a missing mirror is an error.
    pub fn ensure_mirror(&self, name: &str, url: &str) -> Result<PathBuf, Error> {
        check_url(url)?;
        let db = db_dir(&self.git_root, name, url);
        if db.join("HEAD").is_file() || db.join("config").is_file() {
            // Record the URL for `list` even for mirrors predating the marker.
            let marker = db.join("rivet-url.txt");
            if !marker.is_file() {
                let _ = std::fs::write(&marker, url);
            }
            return Ok(db);
        }
        if self.offline {
            return Err(Error::Offline {
                action: format!("fetch `{url}`"),
                where_: db.display().to_string(),
            });
        }
        if let Some(parent) = db.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io("create git db dir", parent, e))?;
        }
        // Clone into a temp sibling then rename (atomic-ish, concurrent-safe).
        let tmp = db.with_extension("tmp-clone");
        if tmp.exists() {
            std::fs::remove_dir_all(&tmp)
                .map_err(|e| Error::io("remove stale clone tmp", &tmp, e))?;
        }
        // Mirrors are full clones (never partial): `git clone --local`
        // checkouts cannot reliably materialize blobs from a
        // `--filter=blob:none` mirror on common git builds (observed:
        // checkout exits 0 with every file deleted). Full mirrors keep
        // checkouts hermetic and offline-capable after the first fetch.
        // Clone into a temp sibling then rename (atomic-ish).
        let tmp = db.with_extension("tmp-clone");
        if tmp.exists() {
            std::fs::remove_dir_all(&tmp)
                .map_err(|e| Error::io("remove stale clone tmp", &tmp, e))?;
        }
        self.run_git(
            &["clone", "--bare", "--", url, &tmp.to_string_lossy()],
            None,
        )
        .map_err(|e| match e {
            Error::GitFailed {
                cmd,
                status,
                stderr,
                ..
            } => Error::GitFailed {
                cmd,
                status,
                stderr,
                hint: format!(
                    "could not clone `{url}`. Hint: check the URL, network, and credentials"
                ),
            },
            other => other,
        })?;
        std::fs::rename(&tmp, &db).map_err(|e| Error::io("rename clone into place", &db, e))?;
        let _ = std::fs::write(db.join("rivet-url.txt"), url);
        Ok(db)
    }

    /// Fetch one ref into an existing mirror (plain fetches; mirrors stay
    /// full so checkouts never depend on promisor behavior).
    fn fetch_ref(&self, db: &Path, url: &str, git_ref: &GitRef) -> Result<(), Error> {
        debug_assert!(!self.offline);
        match git_ref {
            GitRef::Tag(t) => {
                let spec = format!("+refs/tags/{t}:refs/tags/{t}");
                self.run_git_in_db(db, &["fetch", "origin", &spec])
                    .map_err(|e| fetch_hint(e, url, &git_ref.describe()))?;
                Ok(())
            }
            GitRef::Branch(b) => {
                if b == "HEAD" {
                    self.run_git_in_db(db, &["fetch", "origin"])
                        .map_err(|e| fetch_hint(e, url, &git_ref.describe()))?;
                    return Ok(());
                }
                let spec = format!("+refs/heads/{b}:refs/heads/{b}");
                self.run_git_in_db(db, &["fetch", "origin", &spec])
                    .map_err(|e| fetch_hint(e, url, &git_ref.describe()))?;
                Ok(())
            }
            GitRef::Rev(sha) => {
                // Skip the fetch when the object is already present.
                if self.run_git_in_db(db, &["cat-file", "-e", sha]).is_ok() {
                    return Ok(());
                }
                self.run_git_in_db(db, &["fetch", "origin", sha])
                    .map_err(|e| fetch_hint(e, url, &git_ref.describe()))?;
                Ok(())
            }
        }
    }

    /// Resolve `name`/`url`/`ref` to a full SHA (fetching unless offline).
    pub fn resolve_ref(&self, name: &str, url: &str, git_ref: &GitRef) -> Result<String, Error> {
        check_url(url)?;
        let db = self.ensure_mirror(name, url)?;
        if !self.offline {
            // Best-effort freshness: a failing fetch still falls through to
            // local resolution so a warm mirror degrades gracefully (the
            // resolve below errors when the ref is truly absent).
            let _ = self.fetch_ref(&db, url, git_ref);
        }
        self.resolve_local(name, url, &db, git_ref)
    }

    fn resolve_local(
        &self,
        name: &str,
        url: &str,
        db: &Path,
        git_ref: &GitRef,
    ) -> Result<String, Error> {
        let _ = name;
        match git_ref {
            GitRef::Tag(t) => {
                // Peel annotated tags to the commit (`tag^{commit}`).
                let direct = format!("{t}^{{commit}}");
                let out = self
                    .run_git_in_db(db, &["rev-parse", &direct])
                    .or_else(|_| {
                        self.run_git_in_db(db, &["rev-parse", &format!("refs/tags/{t}^{{commit}}")])
                    })
                    .map_err(|_| Error::Unresolvable {
                        what: git_ref.describe(),
                        url: url.to_string(),
                        detail: "tag not found in the local mirror".to_string(),
                        hint: if self.offline {
                            "the tag is not cached; run once online to populate the mirror"
                                .to_string()
                        } else {
                            "check the tag name (did it move?) or run with -v".to_string()
                        },
                    })?;
                validate_sha(url, &out)?;
                Ok(out)
            }
            GitRef::Branch(b) => {
                if b == "HEAD" {
                    let out = self
                        .run_git_in_db(db, &["rev-parse", "HEAD^{commit}"])
                        .map_err(|_| Error::Unresolvable {
                            what: "default branch (HEAD)".to_string(),
                            url: url.to_string(),
                            detail: "mirror has no HEAD".to_string(),
                            hint: if self.offline {
                                "the mirror is missing; run once online".to_string()
                            } else {
                                "the remote may be empty; check the URL".to_string()
                            },
                        })?;
                    validate_sha(url, &out)?;
                    return Ok(out);
                }
                let out = self
                    .run_git_in_db(db, &["rev-parse", &format!("refs/heads/{b}^{{commit}}")])
                    .or_else(|_| {
                        self.run_git_in_db(
                            db,
                            &["rev-parse", &format!("refs/remotes/origin/{b}^{{commit}}")],
                        )
                    })
                    .or_else(|_| self.run_git_in_db(db, &["rev-parse", &format!("{b}^{{commit}}")]))
                    .map_err(|_| Error::Unresolvable {
                        what: git_ref.describe(),
                        url: url.to_string(),
                        detail: "branch not found in the local mirror".to_string(),
                        hint: if self.offline {
                            "the branch is not cached; run once online to populate the mirror"
                                .to_string()
                        } else {
                            "check the branch name or run with -v".to_string()
                        },
                    })?;
                validate_sha(url, &out)?;
                Ok(out)
            }
            GitRef::Rev(sha) => {
                validate_sha(url, sha)?;
                self.run_git_in_db(db, &["cat-file", "-e", sha])
                    .map_err(|_| Error::Unresolvable {
                        what: git_ref.describe(),
                        url: url.to_string(),
                        detail: "commit not present in the local mirror".to_string(),
                        hint: if self.offline {
                            "the commit is not cached; run once online to fetch it".to_string()
                        } else {
                            "check the SHA (did upstream rewrite history?)".to_string()
                        },
                    })?;
                // Normalize (expands abbreviations server-side if any).
                let out = self
                    .run_git_in_db(db, &["rev-parse", &format!("{sha}^{{commit}}")])
                    .unwrap_or_else(|_| sha.clone());
                validate_sha(url, &out)?;
                Ok(out)
            }
        }
    }

    /// `git rev-parse <sha>^{tree}` in the mirror for `(name, url)`.
    pub fn tree_hash(&self, name: &str, url: &str, sha: &str) -> Result<String, Error> {
        check_url(url)?;
        validate_sha(url, sha)?;
        let db = self.ensure_mirror(name, url)?;
        self.run_git_in_db(&db, &["rev-parse", &format!("{sha}^{{tree}}")])
            .map(|s| s.trim().to_string())
            .map_err(|_| Error::Unresolvable {
                what: format!("tree of {sha}"),
                url: url.to_string(),
                detail: "commit has no tree in the local mirror".to_string(),
                hint: "re-resolve the ref; the history may have been rewritten".to_string(),
            })
    }

    /// Materialize an immutable checkout at `sha`; returns the usable dir.
    ///
    /// Returns `<checkout>/<subdir>` when `opts.subdir` is set (validated),
    /// else the checkout root. Reuses an existing checkout when its HEAD
    /// already equals `sha`; verifies `expected_tree` when given; gates
    /// submodule URLs before init; rejects symlinks escaping the root.
    pub fn ensure_checkout(
        &self,
        name: &str,
        url: &str,
        sha: &str,
        opts: &CheckoutOpts,
    ) -> Result<PathBuf, Error> {
        check_url(url)?;
        validate_sha(url, sha)?;
        if let Some(sub) = &opts.subdir {
            validate_subdir(url, sub)?;
        }
        let db = self.ensure_mirror(name, url)?;
        // Object must exist locally (offline-safe: clone below is --local).
        self.run_git_in_db(&db, &["cat-file", "-e", sha])
            .map_err(|_| Error::Unresolvable {
                what: format!("rev \"{sha}\""),
                url: url.to_string(),
                detail: "commit not present in the local mirror".to_string(),
                hint: if self.offline {
                    "run once online to fetch the commit".to_string()
                } else {
                    "resolve the ref first, then check out the resolved SHA".to_string()
                },
            })?;
        let dest = checkout_dir(&self.git_root, name, url, sha);
        if dest.is_dir() {
            let head = self
                .run_git(&["rev-parse", "HEAD"], Some(&dest))
                .unwrap_or_default();
            if head == sha {
                // Heal corrupt checkouts (e.g. file-less trees from the
                // partial-mirror era, or tampered dirs): rebuild below.
                if self.worktree_clean(&dest)? {
                    return self.finish_checkout(url, sha, &dest, opts, true);
                }
                std::fs::remove_dir_all(&dest)
                    .map_err(|e| Error::io("remove corrupt checkout", &dest, e))?;
            } else {
                return Err(Error::Io {
                    action: "reuse checkout".to_string(),
                    path: dest.display().to_string(),
                    message: format!(
                        "checkout exists but HEAD is `{head}`, expected `{sha}`. Hint: remove the directory or run `git prune` and retry"
                    ),
                });
            }
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io("create checkouts dir", parent, e))?;
        }
        let tmp = dest.with_extension("tmp-checkout");
        self.clone_detached(&db, &tmp, sha)?;
        if !self.worktree_clean(&tmp)? {
            // Blobs missing: the mirror predates full-mirror policy (a
            // `blob:none` partial whose promisor cannot satisfy local
            // clones — observed as exit-0 checkouts with every file `D`).
            // Partial fetches never backfill (`partialclonefilter`
            // persists), so drop the mirror and re-clone it full, then
            // redo the working copy exactly once.
            if self.offline {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(Error::Offline {
                    action: format!("materialize checkout of `{url}`"),
                    where_: dest.display().to_string(),
                });
            }
            std::fs::remove_dir_all(&db).map_err(|e| Error::io("remove partial mirror", &db, e))?;
            let db = self.ensure_mirror(name, url)?;
            self.clone_detached(&db, &tmp, sha)?;
            if !self.worktree_clean(&tmp)? {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(Error::GitFailed {
                    cmd: format!("git checkout --detach {sha}"),
                    status: "materialization check failed".to_string(),
                    stderr: "working tree does not match HEAD".to_string(),
                    hint: "the remote may be corrupt; run `rivet git prune` and retry".to_string(),
                });
            }
        }
        let checkout_result = (|| {
            if opts.submodules {
                self.init_submodules(url, sha, &tmp)?;
            }
            if let Some(expected) = &opts.expected_tree {
                let actual = self
                    .run_git(&["rev-parse", "HEAD^{tree}"], Some(&tmp))
                    .unwrap_or_default();
                if &actual != expected {
                    return Err(Error::TreeMismatch {
                        url: url.to_string(),
                        sha: sha.to_string(),
                        expected: expected.clone(),
                        actual,
                    });
                }
            }
            validate_checkout_tree(&tmp)?;
            Ok(())
        })();
        if let Err(e) = checkout_result {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
        std::fs::rename(&tmp, &dest)
            .map_err(|e| Error::io("rename checkout into place", &dest, e))?;
        self.finish_checkout(url, sha, &dest, opts, false)
    }

    /// Clone the mirror locally without checking out, then detach at
    /// `sha`. Shared by the fresh path and the one repair retry.
    fn clone_detached(&self, db: &Path, tmp: &Path, sha: &str) -> Result<(), Error> {
        if tmp.exists() {
            std::fs::remove_dir_all(tmp)
                .map_err(|e| Error::io("remove stale checkout tmp", tmp, e))?;
        }
        // Local clone from the mirror (no network, offline-safe).
        self.run_git(
            &[
                "clone",
                "--local",
                "--no-hardlinks",
                "--no-checkout",
                "--",
                &db.to_string_lossy(),
                &tmp.to_string_lossy(),
            ],
            None,
        )?;
        self.run_git(&["checkout", "--detach", sha], Some(tmp))?;
        Ok(())
    }

    /// `true` when the working tree matches HEAD with no changes.
    ///
    /// Guards against file-less checkouts (a partial-mirror `clone --local`
    /// can exit 0 while materializing nothing) and against tampering with
    /// immutable dirs. Submodule states are ignored (reconciled separately).
    fn worktree_clean(&self, work: &Path) -> Result<bool, Error> {
        let out = self.run_git(
            &[
                "status",
                "--porcelain",
                "--untracked-files=no",
                "--ignore-submodules=all",
            ],
            Some(work),
        )?;
        Ok(out.trim().is_empty())
    }

    fn finish_checkout(
        &self,
        url: &str,
        sha: &str,
        dest: &Path,
        opts: &CheckoutOpts,
        reused: bool,
    ) -> Result<PathBuf, Error> {
        // Reused checkouts were created with whatever flags the first caller
        // used; bring submodule state up to the current request (idempotent)
        // so resolve-then-build with the same dep flags stays consistent.
        // Callers must still use consistent flags per SHA (the resolver and
        // CLI thread the dep's `submodules` flag through everywhere).
        if reused && opts.submodules {
            self.init_submodules(url, sha, dest)?;
        }
        if let Some(sub) = &opts.subdir {
            let full = dest.join(sub);
            if !full.is_dir() {
                return Err(Error::BadSubdir {
                    url: url.to_string(),
                    subdir: sub.clone(),
                    reason: "directory does not exist in the checkout".to_string(),
                });
            }
            // Re-validate symlinks on reuse too (cheap relative to a build).
            validate_checkout_tree(dest)?;
            return Ok(full);
        }
        Ok(dest.to_path_buf())
    }

    /// Clone a repo directory into a writable working copy (`rivet dev`):
    /// `git clone --local <src> <dst>`, then check out `branch` when given.
    ///
    /// The copy is a full clone (all refs), so edits can be committed and
    /// pushed. `dst` must not exist.
    pub fn clone_writable(
        &self,
        src: &Path,
        dst: &Path,
        branch: Option<&str>,
    ) -> Result<(), Error> {
        if dst.exists() {
            return Err(Error::io(
                "prepare dev copy",
                dst,
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "{} already exists. Hint: remove it or pick another `rivet dev` target",
                        dst.display()
                    ),
                ),
            ));
        }
        if let Some(parent) = dst.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::io("create dev parent dir", parent, e))?;
            }
        }
        let src_s = src.to_string_lossy().to_string();
        let dst_s = dst.to_string_lossy().to_string();
        self.run_git(&["clone", "--local", "--", &src_s, &dst_s], None)?;
        if let Some(b) = branch {
            if b != "HEAD" {
                self.run_git(&["checkout", b], Some(dst)).map_err(|e| match e {
                    Error::GitFailed {
                        cmd,
                        status,
                        stderr,
                        ..
                    } => Error::GitFailed {
                        cmd,
                        status,
                        stderr,
                        hint: format!(
                            "could not check out branch `{b}` in the dev copy. Hint: the branch may not exist upstream; create it with `git checkout -b {b}`"
                        ),
                    },
                    other => other,
                })?;
            }
        }
        Ok(())
    }

    fn init_submodules(&self, url: &str, sha: &str, work: &Path) -> Result<(), Error> {
        let urls = submodule_urls_in_checkout(work)?;
        for (sub_name, sub_url) in &urls {
            if !is_allowed_url(sub_url) {
                return Err(Error::BadSubmoduleUrl {
                    url: url.to_string(),
                    sha: sha.to_string(),
                    name: sub_name.clone(),
                    sub_url: sub_url.clone(),
                });
            }
        }
        if urls.is_empty() {
            return Ok(());
        }
        self.run_git(
            &["submodule", "update", "--init", "--recursive"],
            Some(work),
        )
        .map_err(|e| match e {
            Error::GitFailed {
                cmd,
                status,
                stderr,
                ..
            } => Error::GitFailed {
                cmd,
                status,
                stderr,
                hint: "submodule init failed; check submodule URLs and network access".to_string(),
            },
            other => other,
        })?;
        Ok(())
    }

    /// Verify the locked tag still points at `locked_sha`.
    ///
    /// Resolves the tag fresh (fetching unless offline) and errors with
    /// [`Error::MovedTag`] when it drifted.
    pub fn verify_tag_pinned(
        &self,
        name: &str,
        url: &str,
        tag: &str,
        locked_sha: &str,
    ) -> Result<String, Error> {
        let current = self.resolve_ref(name, url, &GitRef::Tag(tag.to_string()))?;
        check_tag_not_moved(url, tag, locked_sha, &current).map_err(Error::MovedTag)?;
        Ok(current)
    }

    /// `git rev-list --count <old>..<new>` in the mirror (update messages).
    pub fn head_count_between(
        &self,
        name: &str,
        url: &str,
        old_sha: &str,
        new_sha: &str,
    ) -> Result<u64, Error> {
        check_url(url)?;
        let db = self.ensure_mirror(name, url)?;
        head_count_between(&db, old_sha, new_sha, self.verbose)
    }

    /// Fetch/update mirrors for every `(name, url)` pair.
    pub fn fetch_all(&self, deps: &[(&str, &str)]) -> Result<(), Error> {
        for (name, url) in deps {
            check_url(url)?;
            let db = self.ensure_mirror(name, url)?;
            if self.offline {
                continue;
            }
            self.run_git_in_db(&db, &["fetch", "origin"])?;
        }
        Ok(())
    }

    /// Inventory of mirrors + checkouts with sizes/mtimes (CLI renders this).
    pub fn list_info(&self) -> Result<CacheInventory, Error> {
        let mut inv = CacheInventory::default();
        let db_root = self.git_root.join("db");
        if db_root.is_dir() {
            let mut entries = read_dir_sorted(&db_root)?;
            entries.sort();
            for e in entries {
                let db = db_root.join(&e);
                if !db.is_dir() {
                    continue;
                }
                let url = std::fs::read_to_string(db.join("rivet-url.txt"))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();
                inv.remotes.push(RemoteInfo {
                    name: e,
                    url,
                    path: db.clone(),
                    size_bytes: dir_size(&db),
                    mtime: mtime_of(&db),
                });
            }
        }
        let co_root = self.git_root.join("checkouts");
        if co_root.is_dir() {
            for group in read_dir_sorted(&co_root)? {
                let base = co_root.join(&group);
                if !base.is_dir() {
                    continue;
                }
                for sha_dir in read_dir_sorted(&base)? {
                    let path = base.join(&sha_dir);
                    if !path.is_dir() {
                        continue;
                    }
                    let full_sha = self
                        .run_git(&["rev-parse", "HEAD"], Some(&path))
                        .unwrap_or_else(|_| sha_dir.clone());
                    inv.checkouts.push(CheckoutInfo {
                        group: group.clone(),
                        sha: full_sha,
                        short_sha: sha_dir,
                        path: path.clone(),
                        size_bytes: dir_size(&path),
                        mtime: mtime_of(&path),
                    });
                }
            }
        }
        inv.total_bytes = inv
            .remotes
            .iter()
            .map(|r| r.size_bytes)
            .sum::<u64>()
            .saturating_add(inv.checkouts.iter().map(|c| c.size_bytes).sum::<u64>());
        inv.remotes.sort_by(|a, b| a.name.cmp(&b.name));
        inv.checkouts.sort_by(|a, b| {
            a.group
                .cmp(&b.group)
                .then_with(|| a.short_sha.cmp(&b.short_sha))
        });
        Ok(inv)
    }

    /// Which checkouts/mirrors would be pruned given live SHAs (dry-run data).
    ///
    /// `live_shas` holds full commit SHAs still referenced by a lockfile.
    /// Checkouts whose HEAD is not live are unused; mirrors with no
    /// checkouts left on disk are reported as reclaimable.
    pub fn prune_info(&self, live_shas: &HashSet<String>) -> Result<PrunePlan, Error> {
        let inv = self.list_info()?;
        let mut plan = PrunePlan::default();
        for c in &inv.checkouts {
            if !live_shas.contains(&c.sha) && !live_shas.iter().any(|s| s.starts_with(&c.short_sha))
            {
                plan.unused_checkouts.push(c.path.clone());
                plan.reclaimable_bytes = plan.reclaimable_bytes.saturating_add(c.size_bytes);
            }
        }
        let live_groups: HashSet<&str> = inv
            .checkouts
            .iter()
            .filter(|c| plan.unused_checkouts.iter().all(|p| p != &c.path))
            .map(|c| c.group.as_str())
            .collect();
        for r in &inv.remotes {
            if !live_groups.contains(r.name.as_str()) {
                plan.unused_remotes.push(r.path.clone());
                plan.reclaimable_bytes = plan.reclaimable_bytes.saturating_add(r.size_bytes);
            }
        }
        plan.unused_checkouts.sort();
        plan.unused_remotes.sort();
        Ok(plan)
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (also used by tests / the resolver)
// ---------------------------------------------------------------------------

fn fetch_hint(err: Error, url: &str, what: &str) -> Error {
    match err {
        Error::GitFailed {
            cmd, status, stderr, ..
        } => Error::GitFailed {
            cmd,
            status,
            stderr,
            hint: format!(
                "could not fetch {what} from `{url}`. Hint: check the ref name, network, and credentials"
            ),
        },
        other => other,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    format!("{}…[truncated]", &s[..max])
}

/// Pure moved-tag check: `Err(MovedTag)` unless SHAs match.
pub fn check_tag_not_moved(
    url: &str,
    tag: &str,
    locked_sha: &str,
    current_sha: &str,
) -> Result<(), MovedTag> {
    if locked_sha == current_sha {
        Ok(())
    } else {
        Err(MovedTag {
            url: url.to_string(),
            tag: tag.to_string(),
            locked_sha: locked_sha.to_string(),
            current_sha: current_sha.to_string(),
        })
    }
}

/// `git --git-dir=<db> rev-list --count <old>..<new>`.
///
/// Uses a shared temp `empty-hooks` dir (never the caller's cwd) so the
/// single `run_git` semantics (hooks disabled, no prompts) hold without
/// touching the filesystem outside the temp dir and the mirror.
pub fn head_count_between(
    db: &Path,
    old_sha: &str,
    new_sha: &str,
    verbose: bool,
) -> Result<u64, Error> {
    if old_sha == new_sha {
        return Ok(0);
    }
    let range = format!("{old_sha}..{new_sha}");
    let db_str = db.to_string_lossy().to_string();
    let out = run_git_raw(
        &["--git-dir", &db_str, "rev-list", "--count", &range],
        None,
        verbose,
    )
    .map_err(|e| match e {
        Error::GitFailed {
            cmd,
            status,
            stderr,
            ..
        } => Error::GitFailed {
            cmd,
            status,
            stderr,
            hint: "rev-list failed; check both SHAs exist in the mirror".to_string(),
        },
        other => other,
    })?;
    out.trim().parse::<u64>().map_err(|_| Error::GitFailed {
        cmd: format!("git rev-list --count {range}"),
        status: "parse".to_string(),
        stderr: format!("unexpected rev-list output `{out}`"),
        hint: "the mirror may be corrupt; prune and re-fetch".to_string(),
    })
}

/// `git -C <checkout> rev-parse HEAD^{tree}`.
///
/// Uses a shared temp `empty-hooks` dir (see [`head_count_between`]).
pub fn tree_hash_of_checkout(checkout: &Path, verbose: bool) -> Result<String, Error> {
    let out = run_git_raw(&["rev-parse", "HEAD^{tree}"], Some(checkout), verbose)?;
    Ok(out.trim().to_string())
}

/// Shared temp dir carrying an (empty) hooks dir for the free helpers.
fn fallback_hooks_dir() -> Result<PathBuf, Error> {
    let dir = std::env::temp_dir().join("rivet-empty-hooks");
    if !dir.is_dir() {
        std::fs::create_dir_all(&dir).map_err(|e| Error::io("create temp hooks dir", &dir, e))?;
    }
    Ok(dir)
}

/// Minimal `git` runner for the free helpers (same env/logging contract as
/// [`GitCache::run_git`] but rooted at a temp hooks dir).
fn run_git_raw(args: &[&str], cwd: Option<&Path>, verbose: bool) -> Result<String, Error> {
    require_git()?;
    let hooks = fallback_hooks_dir()?;
    let mut cmd = std::process::Command::new("git");
    let mut hooks_arg = OsString::from("core.hooksPath=");
    hooks_arg.push(&hooks);
    cmd.arg("-c");
    cmd.arg(hooks_arg);
    cmd.arg("-c");
    cmd.arg("protocol.file.allow=always");
    for a in args {
        cmd.arg(a);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if verbose {
        let mut line = String::from("+ git");
        for a in args {
            line.push(' ');
            line.push_str(a);
        }
        eprintln!("{line}");
    }
    let output = cmd.output().map_err(|e| Error::Io {
        action: "spawn `git`".to_string(),
        path: "git".to_string(),
        message: format!("{e}. Hint: install git from https://git-scm.com"),
    })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = truncate(&String::from_utf8_lossy(&output.stderr), 2000);
        let mut cmdline = String::from("git");
        for a in args {
            cmdline.push(' ');
            cmdline.push_str(a);
        }
        Err(Error::GitFailed {
            cmd: cmdline,
            status: format!("{}", output.status),
            stderr,
            hint: "run with -v to see the full command; check the URL, ref, and network access"
                .to_string(),
        })
    }
}

/// Gate a batch of submodule URLs with [`check_url`].
pub fn submodule_urls_ok(url: &str, sha: &str, urls: &[String]) -> Result<(), Error> {
    for sub in urls {
        if !is_allowed_url(sub) {
            return Err(Error::BadSubmoduleUrl {
                url: url.to_string(),
                sha: sha.to_string(),
                name: String::new(),
                sub_url: sub.clone(),
            });
        }
    }
    Ok(())
}

/// Parse `.gitmodules` text into `(name, url)` pairs (minimal INI scan).
pub fn parse_gitmodules_urls(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("[submodule") {
            current = rest
                .trim_matches(|c| c == '[' || c == ']' || c == '"' || c == ' ')
                .to_string();
            // Format is `[submodule "name"]`; extract the quoted part.
            if let Some(a) = line.find('"') {
                if let Some(b) = line[a + 1..].find('"') {
                    current = line[a + 1..a + 1 + b].to_string();
                }
            }
            continue;
        }
        let mut parts = t.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let val = parts
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .to_string();
        if key == "url" && !val.is_empty() {
            out.push((current.clone(), val));
        }
    }
    out
}

/// Submodule `(name, url)` pairs declared in a materialized checkout.
pub fn submodule_urls_in_checkout(work: &Path) -> Result<Vec<(String, String)>, Error> {
    let file = work.join(".gitmodules");
    if !file.is_file() {
        return Ok(Vec::new());
    }
    let text =
        std::fs::read_to_string(&file).map_err(|e| Error::io("read .gitmodules", &file, e))?;
    Ok(parse_gitmodules_urls(&text))
}

/// Walk `root` (excluding `.git/`) and reject symlinks escaping the root.
///
/// Also rejects absolute symlink targets. `..`-style escapes are caught by
/// lexically normalizing `parent.join(target)` and requiring the result to
/// stay under `root`.
pub fn validate_checkout_tree(root: &Path) -> Result<(), Error> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| Error::io("list checkout dir", &dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| Error::io("list checkout entry", &dir, e))?;
            let path = entry.path();
            let ft = entry
                .file_type()
                .map_err(|e| Error::io("stat checkout entry", &path, e))?;
            if ft.is_symlink() {
                let target =
                    std::fs::read_link(&path).map_err(|e| Error::io("read symlink", &path, e))?;
                if target.is_absolute() {
                    return Err(Error::UnsafePath {
                        root: root.display().to_string(),
                        path: path.display().to_string(),
                        reason: format!("symlink points at absolute target `{}`", target.display()),
                    });
                }
                let parent = path.parent().unwrap_or(root);
                let joined = parent.join(&target);
                let normalized = lexically_normalize(&joined);
                if !normalized.starts_with(root) {
                    return Err(Error::UnsafePath {
                        root: root.display().to_string(),
                        path: path.display().to_string(),
                        reason: format!(
                            "symlink escapes the checkout root (`{}`)",
                            target.display()
                        ),
                    });
                }
                continue;
            }
            if ft.is_dir() {
                if path.file_name().is_some_and(|n| n == ".git") && path.parent() == Some(root) {
                    continue; // never descend into the clone metadata
                }
                stack.push(path);
            }
        }
    }
    Ok(())
}

/// Lexical `..`/`.` normalization without touching the filesystem.
fn lexically_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| Error::io("list cache dir", dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io("list cache entry", dir, e))?;
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let p = entry.path();
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(p);
            } else if let Ok(md) = entry.metadata() {
                total = total.saturating_add(md.len());
            }
        }
    }
    total
}

fn mtime_of(path: &Path) -> Option<String> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs().to_string())
        })
}

// ---------------------------------------------------------------------------
// Inventory types (data providers for future CLI rendering)
// ---------------------------------------------------------------------------

/// One bare mirror under `db/`.
#[derive(Debug, Clone)]
pub struct RemoteInfo {
    /// `<name>-<urlhash>` directory name.
    pub name: String,
    /// URL recorded in `rivet-url.txt` (empty for pre-marker mirrors).
    pub url: String,
    /// Mirror path.
    pub path: PathBuf,
    /// Recursive size in bytes (symlinks skipped).
    pub size_bytes: u64,
    /// Directory mtime as unix-seconds string (`None` when unavailable).
    pub mtime: Option<String>,
}

/// One immutable checkout under `checkouts/<group>/<short-sha>/`.
#[derive(Debug, Clone)]
pub struct CheckoutInfo {
    /// `<name>-<urlhash>` group directory name.
    pub group: String,
    /// Full HEAD SHA.
    pub sha: String,
    /// Directory name (12-char prefix).
    pub short_sha: String,
    /// Checkout path.
    pub path: PathBuf,
    /// Recursive size in bytes (excluding `.git`).
    pub size_bytes: u64,
    /// Directory mtime as unix-seconds string.
    pub mtime: Option<String>,
}

/// Full cache inventory for `git list`-style rendering.
#[derive(Debug, Clone, Default)]
pub struct CacheInventory {
    /// Bare mirrors.
    pub remotes: Vec<RemoteInfo>,
    /// Immutable checkouts.
    pub checkouts: Vec<CheckoutInfo>,
    /// Sum of all reported sizes.
    pub total_bytes: u64,
}

/// Dry-run prune plan for `git prune`-style rendering.
#[derive(Debug, Clone, Default)]
pub struct PrunePlan {
    /// Checkout paths safe to delete.
    pub unused_checkouts: Vec<PathBuf>,
    /// Mirror paths with no surviving checkouts.
    pub unused_remotes: Vec<PathBuf>,
    /// Bytes that deletion would reclaim.
    pub reclaimable_bytes: u64,
}

/// Returns `Ok(())` if a `git` binary is discoverable, else [`Error::GitNotFound`].
pub fn require_git() -> Result<(), Error> {
    // Deliberately avoid the `which` crate here to keep this crate dep-free;
    // PATH probing is trivial and fully testable.
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut found = false;
    for dir in std::env::split_paths(&path) {
        #[cfg(windows)]
        {
            if dir.join("git.exe").is_file() {
                found = true;
                break;
            }
        }
        #[cfg(not(windows))]
        {
            if dir.join("git").is_file() {
                found = true;
                break;
            }
        }
    }
    if found {
        Ok(())
    } else {
        Err(Error::GitNotFound)
    }
}

// ---------------------------------------------------------------------------
// Tests (tempdirs + `file://` bare repos only; no network, ever)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn git_available() -> bool {
        require_git().is_ok()
    }

    /// Convert a temp path to a `file://` URL (forward slashes for Windows).
    fn file_url(p: &Path) -> String {
        let s = p.display().to_string().replace('\\', "/");
        if s.starts_with('/') {
            format!("file://{s}")
        } else {
            format!("file:///{s}")
        }
    }

    struct Repo {
        _dir: tempfile::TempDir,
        _bare: PathBuf,
        url: String,
    }

    /// Create a bare repo with two commits + tag `v1` on the first.
    fn make_repo(tag_first: bool) -> Result<Repo, Error> {
        let dir = tempfile::TempDir::new().map_err(|e| Error::Io {
            action: "create tempdir".to_string(),
            path: "<tempdir>".to_string(),
            message: e.to_string(),
        })?;
        let bare = dir.path().join("origin.git");
        let work = dir.path().join("work");
        let cache = GitCache::from_git_root(dir.path().join("cache-git"));
        cache.run_git(&["init", "--bare", "--", &bare.to_string_lossy()], None)?;
        std::fs::create_dir_all(&work).map_err(|e| Error::io("create workdir", &work, e))?;
        cache.run_git(&["init", "--", &work.to_string_lossy()], None)?;
        cache.run_git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "config",
                "user.email",
                "t@t",
            ],
            Some(&work),
        )?;
        cache.run_git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "config",
                "user.name",
                "t",
            ],
            Some(&work),
        )?;
        std::fs::write(work.join("a.txt"), "one\n")
            .map_err(|e| Error::io("write", &work.join("a.txt"), e))?;
        cache.run_git(&["add", "a.txt"], Some(&work))?;
        cache.run_git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "one",
            ],
            Some(&work),
        )?;
        if tag_first {
            cache.run_git(&["tag", "v1"], Some(&work))?;
        }
        let v1_sha = cache.run_git(&["rev-parse", "HEAD"], Some(&work))?;
        std::fs::write(work.join("a.txt"), "two\n")
            .map_err(|e| Error::io("write", &work.join("a.txt"), e))?;
        cache.run_git(&["add", "a.txt"], Some(&work))?;
        cache.run_git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "two",
            ],
            Some(&work),
        )?;
        if !tag_first {
            cache.run_git(&["tag", "v1", &v1_sha], Some(&work))?;
        }
        let branch = cache
            .run_git(&["branch", "--show-current"], Some(&work))
            .unwrap_or_default();
        let branch = if branch.is_empty() {
            "master"
        } else {
            branch.as_str()
        };
        cache.run_git(
            &["remote", "add", "origin", &bare.to_string_lossy()],
            Some(&work),
        )?;
        cache.run_git(&["push", "origin", branch, "--tags"], Some(&work))?;
        let url = file_url(&bare);
        Ok(Repo {
            _dir: dir,
            _bare: bare,
            url,
        })
    }

    fn temp_cache() -> (tempfile::TempDir, GitCache) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cache = GitCache::from_git_root(dir.path().join("git"));
        (dir, cache)
    }

    #[test]
    fn require_git_ok_when_present() {
        if !git_available() {
            eprintln!("SKIP: git not in PATH");
            return;
        }
        assert!(require_git().is_ok());
    }

    #[test]
    fn url_gate_allows_expected_schemes() {
        assert!(is_allowed_url("https://github.com/x/y.git"));
        assert!(is_allowed_url("ssh://git@github.com/x/y.git"));
        assert!(is_allowed_url("file:///tmp/x.git"));
        assert!(is_allowed_url("git@github.com:x/y.git"));
        assert!(check_url("https://github.com/x/y.git").is_ok());
    }

    #[test]
    fn url_gate_rejects_others() {
        for bad in [
            "http://github.com/x/y.git",
            "ftp://x/y.git",
            "git://github.com/x/y.git",
            "C:\\repos\\x.git",
            "/tmp/x.git",
            "github.com/x/y.git",
        ] {
            assert!(!is_allowed_url(bad), "should reject {bad}");
            assert!(check_url(bad).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn tag_resolves_and_pins() {
        if !git_available() {
            eprintln!("SKIP: no git");
            return;
        }
        let repo = make_repo(true).expect("repo");
        let (_d, cache) = temp_cache();
        let sha = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Tag("v1".to_string()))
            .expect("resolve tag");
        assert_eq!(sha.len(), 40);
        // Pinning the resolved SHA resolves to itself.
        let again = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Rev(sha.clone()))
            .expect("resolve rev");
        assert_eq!(again, sha);
    }

    #[test]
    fn branch_advance_and_count() {
        if !git_available() {
            eprintln!("SKIP: no git");
            return;
        }
        let repo = make_repo(false).expect("repo");
        let (_d, cache) = temp_cache();
        let v1 = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Tag("v1".to_string()))
            .expect("v1");
        // Default branch HEAD should be the second commit (one ahead of v1).
        let head = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Branch("HEAD".to_string()))
            .expect("head");
        assert_ne!(v1, head);
        let n = cache
            .head_count_between("pkg", &repo.url, &v1, &head)
            .expect("count");
        assert_eq!(n, 1);
        assert_eq!(
            head_count_between(&cache.mirror_path("pkg", &repo.url), &head, &head, false)
                .expect("same"),
            0
        );
    }

    #[test]
    fn moved_tag_detected() {
        if !git_available() {
            eprintln!("SKIP: no git");
            return;
        }
        let repo = make_repo(true).expect("repo");
        let (_d, cache) = temp_cache();
        let locked = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Tag("v1".to_string()))
            .expect("v1");
        // Pure check passes when SHAs match.
        assert!(check_tag_not_moved(&repo.url, "v1", &locked, &locked).is_ok());
        // And reports MovedTag otherwise.
        let moved = check_tag_not_moved(&repo.url, "v1", &locked, &"0".repeat(40));
        assert!(moved.is_err());
        let m = moved.unwrap_err();
        assert_eq!(m.tag, "v1");
        // Move the tag upstream to HEAD and verify the cache notices.
        let head = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Branch("HEAD".to_string()))
            .expect("head");
        assert_ne!(locked, head);
        // Retag v1 at HEAD in the origin bare repo.
        let work = tempfile::TempDir::new().expect("tempdir");
        cache
            .run_git(
                &[
                    "clone",
                    "--",
                    &repo.url,
                    &work.path().join("c").to_string_lossy(),
                ],
                None,
            )
            .expect("clone");
        let c = work.path().join("c");
        cache
            .run_git(&["tag", "-f", "v1", &head], Some(&c))
            .expect("retag");
        cache
            .run_git(&["push", "-f", "origin", "tag", "v1"], Some(&c))
            .expect("push tag");
        let err = cache
            .verify_tag_pinned("pkg", &repo.url, "v1", &locked)
            .expect_err("moved tag must error");
        assert!(matches!(err, Error::MovedTag(_)), "got: {err}");
    }

    #[test]
    fn rev_pin_and_tree_hash() {
        if !git_available() {
            eprintln!("SKIP: no git");
            return;
        }
        let repo = make_repo(true).expect("repo");
        let (_d, cache) = temp_cache();
        let sha = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Branch("HEAD".to_string()))
            .expect("head");
        let tree = cache.tree_hash("pkg", &repo.url, &sha).expect("tree");
        assert_eq!(tree.len(), 40);
        let opts = CheckoutOpts::default();
        let co = cache
            .ensure_checkout("pkg", &repo.url, &sha, &opts)
            .expect("checkout");
        assert!(co.is_dir());
        let tree2 = tree_hash_of_checkout(&co, false).expect("tree of checkout");
        assert_eq!(tree, tree2);
        // Wrong expected tree is an error (use a fresh sha dir to force verify).
        let bad_sha = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Tag("v1".to_string()))
            .expect("v1");
        let bad = cache.ensure_checkout(
            "other",
            &repo.url,
            &bad_sha,
            &CheckoutOpts {
                expected_tree: Some("0".repeat(40)),
                ..Default::default()
            },
        );
        assert!(
            matches!(bad, Err(Error::TreeMismatch { .. })),
            "got: {bad:?}"
        );
    }

    #[test]
    fn subdir_export() {
        if !git_available() {
            eprintln!("SKIP: no git");
            return;
        }
        let dir = tempfile::TempDir::new().expect("tempdir");
        let bare = dir.path().join("o.git");
        let work = dir.path().join("w");
        let cache = GitCache::from_git_root(dir.path().join("g"));
        cache
            .run_git(&["init", "--bare", "--", &bare.to_string_lossy()], None)
            .expect("bare");
        std::fs::create_dir_all(work.join("sub/inner")).expect("dirs");
        cache
            .run_git(&["init", "--", &work.to_string_lossy()], None)
            .expect("init");
        std::fs::write(work.join("sub/inner/f.txt"), "x\n").expect("write");
        std::fs::write(work.join("top.txt"), "t\n").expect("write");
        cache.run_git(&["add", "."], Some(&work)).expect("add");
        cache
            .run_git(
                &[
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-m",
                    "i",
                ],
                Some(&work),
            )
            .expect("commit");
        let branch = cache
            .run_git(&["branch", "--show-current"], Some(&work))
            .unwrap_or_default();
        let branch = if branch.is_empty() {
            "master"
        } else {
            branch.as_str()
        };
        cache
            .run_git(
                &["remote", "add", "origin", &bare.to_string_lossy()],
                Some(&work),
            )
            .expect("remote");
        cache
            .run_git(&["push", "origin", branch], Some(&work))
            .expect("push");
        let url = file_url(&bare);
        let sha = cache
            .resolve_ref("p", &url, &GitRef::Branch("HEAD".to_string()))
            .expect("sha");
        let sub = cache
            .ensure_checkout(
                "p",
                &url,
                &sha,
                &CheckoutOpts {
                    subdir: Some("sub/inner".to_string()),
                    ..Default::default()
                },
            )
            .expect("subdir checkout");
        assert!(sub.join("f.txt").is_file());
        // Traversal subdir rejected.
        let bad = cache.ensure_checkout(
            "p",
            &url,
            &sha,
            &CheckoutOpts {
                subdir: Some("../evil".to_string()),
                ..Default::default()
            },
        );
        assert!(matches!(bad, Err(Error::BadSubdir { .. })), "got: {bad:?}");
    }

    #[test]
    fn submodules_gated_and_init() {
        if !git_available() {
            eprintln!("SKIP: no git");
            return;
        }
        // Submodule repo.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let sub_bare = dir.path().join("sub.git");
        let sub_work = dir.path().join("subw");
        let cache = GitCache::from_git_root(dir.path().join("g"));
        {
            let (b, w) = (&sub_bare, &sub_work);
            cache
                .run_git(&["init", "--bare", "--", &b.to_string_lossy()], None)
                .expect("bare");
            std::fs::create_dir_all(w).expect("dir");
            cache
                .run_git(&["init", "--", &w.to_string_lossy()], None)
                .expect("init");
            std::fs::write(w.join("s.txt"), "s\n").expect("w");
            cache.run_git(&["add", "."], Some(w)).expect("add");
            cache
                .run_git(
                    &[
                        "-c",
                        "user.email=t@t",
                        "-c",
                        "user.name=t",
                        "commit",
                        "-m",
                        "s",
                    ],
                    Some(w),
                )
                .expect("commit");
            let br = cache
                .run_git(&["branch", "--show-current"], Some(w))
                .unwrap_or_default();
            let br = if br.is_empty() { "master" } else { br.as_str() };
            cache
                .run_git(&["remote", "add", "origin", &b.to_string_lossy()], Some(w))
                .expect("r");
            cache.run_git(&["push", "origin", br], Some(w)).expect("p");
        }
        // Parent repo with the submodule.
        let par_bare = dir.path().join("par.git");
        let par_work = dir.path().join("parw");
        cache
            .run_git(&["init", "--bare", "--", &par_bare.to_string_lossy()], None)
            .expect("bare");
        std::fs::create_dir_all(&par_work).expect("dir");
        cache
            .run_git(&["init", "--", &par_work.to_string_lossy()], None)
            .expect("init");
        std::fs::write(par_work.join("p.txt"), "p\n").expect("w");
        cache.run_git(&["add", "."], Some(&par_work)).expect("add");
        cache
            .run_git(
                &[
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-m",
                    "p",
                ],
                Some(&par_work),
            )
            .expect("commit");
        let sub_url = file_url(&sub_bare);
        cache
            .run_git(
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    &sub_url,
                    "vendor/sub",
                ],
                Some(&par_work),
            )
            .expect("submodule add");
        cache
            .run_git(
                &[
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "user.name=t",
                    "commit",
                    "-m",
                    "sub",
                ],
                Some(&par_work),
            )
            .expect("commit");
        let br = cache
            .run_git(&["branch", "--show-current"], Some(&par_work))
            .unwrap_or_default();
        let br = if br.is_empty() { "master" } else { br.as_str() };
        cache
            .run_git(
                &["remote", "add", "origin", &par_bare.to_string_lossy()],
                Some(&par_work),
            )
            .expect("r");
        cache
            .run_git(&["push", "origin", br], Some(&par_work))
            .expect("p");
        let par_url = file_url(&par_bare);
        let sha = cache
            .resolve_ref("par", &par_url, &GitRef::Branch("HEAD".to_string()))
            .expect("sha");
        // submodules=false: vendor dir stays empty/uninitialized.
        let plain = cache
            .ensure_checkout("par", &par_url, &sha, &CheckoutOpts::default())
            .expect("plain");
        assert!(!plain.join("vendor/sub/s.txt").is_file());
        // submodules=true: content materializes.
        let full = cache
            .ensure_checkout(
                "par2",
                &par_url,
                &sha,
                &CheckoutOpts {
                    submodules: true,
                    ..Default::default()
                },
            )
            .expect("full");
        assert!(full.join("vendor/sub/s.txt").is_file());
        // Bad-scheme submodule URL is rejected before init.
        let bad = submodule_urls_ok(&par_url, &sha, &["gopher://x/y".to_string()]);
        assert!(matches!(bad, Err(Error::BadSubmoduleUrl { .. })));
    }

    #[test]
    fn traversal_and_symlink_rejected() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let root = dir.path().join("co");
        std::fs::create_dir_all(&root).expect("dir");
        std::fs::write(root.join("ok.txt"), "ok").expect("w");
        // Symlink creation needs privileges on Windows (Developer Mode /
        // SeCreateSymbolicLinkPrivilege); skip gracefully when unavailable.
        let can_symlink = try_symlink_file("real-target", &root.join("probe-link")).is_ok();
        let _ = std::fs::remove_file(root.join("probe-link"));
        if !can_symlink {
            eprintln!("SKIP: cannot create symlinks (missing privilege)");
            // Still exercise the non-symlink path (no false positives).
            validate_checkout_tree(&root).expect("plain tree ok");
            return;
        }
        // Absolute symlink target.
        try_symlink_file(&absolute_target_for_test(), &root.join("evil")).expect("link");
        let err = validate_checkout_tree(&root).expect_err("absolute link must fail");
        assert!(matches!(err, Error::UnsafePath { .. }), "got: {err}");
        std::fs::remove_file(root.join("evil")).expect("rm");
        // Escaping relative link.
        try_symlink_file("../outside.txt", &root.join("evil2")).expect("link");
        let err2 = validate_checkout_tree(&root).expect_err("escaping link must fail");
        assert!(matches!(err2, Error::UnsafePath { .. }), "got: {err2}");
        std::fs::remove_file(root.join("evil2")).expect("rm");
        // Benign relative link inside the root passes.
        std::fs::create_dir_all(root.join("sub")).expect("dir");
        std::fs::write(root.join("sub/real.txt"), "r").expect("w");
        try_symlink_file("real.txt", &root.join("sub/link.txt")).expect("link");
        validate_checkout_tree(&root).expect("benign link ok");
    }

    /// Portable file-symlink helper (unix symlink / windows symlink_file).
    fn try_symlink_file(target: &str, link: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        return std::os::windows::fs::symlink_file(target, link);
    }

    /// An absolute target guaranteed to exist textually (need not exist on disk:
    /// absolute targets are rejected before any existence check).
    fn absolute_target_for_test() -> String {
        #[cfg(unix)]
        return "/etc/passwd".to_string();
        #[cfg(windows)]
        return "C:\\Windows\\System32\\drivers\\etc\\hosts".to_string();
    }

    #[test]
    fn bad_scheme_and_bad_sha_rejected() {
        if !git_available() {
            eprintln!("SKIP: no git");
            return;
        }
        let (_d, cache) = temp_cache();
        let bad = cache.resolve_ref("p", "gopher://x/y.git", &GitRef::Branch("HEAD".to_string()));
        assert!(matches!(bad, Err(Error::BadUrl { .. })), "got: {bad:?}");
        // Bad SHA shape rejected without touching the network.
        let repo = make_repo(true).expect("repo");
        let bad_sha = cache.resolve_ref("p", &repo.url, &GitRef::Rev("zzz".to_string()));
        assert!(
            matches!(bad_sha, Err(Error::InvalidSha { .. })),
            "got: {bad_sha:?}"
        );
    }

    #[test]
    fn tree_mismatch_error_spelling() {
        let err = Error::TreeMismatch {
            url: "https://example.com/r.git".to_string(),
            sha: "a".repeat(40),
            expected: "b".repeat(40),
            actual: "c".repeat(40),
        };
        let msg = err.to_string();
        assert!(msg.contains("tree hash mismatch"), "got: {msg}");
        assert!(msg.contains("rivet update"), "got: {msg}");
    }

    #[test]
    fn list_and_prune_info() {
        if !git_available() {
            eprintln!("SKIP: no git");
            return;
        }
        let repo = make_repo(true).expect("repo");
        let (_d, cache) = temp_cache();
        let v1 = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Tag("v1".to_string()))
            .expect("v1");
        let head = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Branch("HEAD".to_string()))
            .expect("head");
        cache
            .ensure_checkout("pkg", &repo.url, &v1, &CheckoutOpts::default())
            .expect("co1");
        cache
            .ensure_checkout("pkg", &repo.url, &head, &CheckoutOpts::default())
            .expect("co2");
        let inv = cache.list_info().expect("list");
        assert_eq!(inv.remotes.len(), 1);
        assert_eq!(inv.checkouts.len(), 2);
        assert!(inv.total_bytes > 0);
        // Prune with only HEAD live: v1 checkout + nothing else unused remote.
        let mut live = HashSet::new();
        live.insert(head.clone());
        let plan = cache.prune_info(&live).expect("prune");
        assert_eq!(plan.unused_checkouts.len(), 1);
        assert!(plan.unused_remotes.is_empty());
        assert!(plan.reclaimable_bytes > 0);
        // Everything live: nothing to prune.
        live.insert(v1);
        let plan2 = cache.prune_info(&live).expect("prune2");
        assert!(plan2.unused_checkouts.is_empty());
    }

    #[test]
    fn fetch_all_updates_mirror() {
        if !git_available() {
            eprintln!("SKIP: no git");
            return;
        }
        let repo = make_repo(true).expect("repo");
        let (_d, cache) = temp_cache();
        cache
            .fetch_all(&[("pkg", repo.url.as_str())])
            .expect("fetch");
        let sha = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Tag("v1".to_string()))
            .expect("v1");
        assert_eq!(sha.len(), 40);
    }

    #[test]
    fn offline_missing_mirror_errors_actionably() {
        let (_d, cache) = temp_cache();
        let offline = GitCache::from_git_root(cache.git_root().to_path_buf()).with_offline(true);
        let err = offline
            .resolve_ref(
                "pkg",
                "file:///nonexistent.git",
                &GitRef::Branch("HEAD".to_string()),
            )
            .expect_err("offline missing must fail");
        let msg = err.to_string();
        assert!(msg.contains("offline"), "got: {msg}");
    }

    #[test]
    fn gitmodules_parser_reads_urls() {
        let text = "[submodule \"a\"]\n\tpath = a\n\turl = https://example.com/a.git\n[submodule \"b\"]\n\turl = git@github.com:x/b.git\n";
        let urls = parse_gitmodules_urls(text);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].0, "a");
        assert!(is_allowed_url(&urls[0].1));
    }

    #[test]
    fn checkout_materializes_all_files() {
        // Regression: a partial-mirror `clone --local` can exit 0 while
        // materializing nothing (dirs only, every file `D`). Checkouts
        // must contain every tracked file with a clean worktree.
        if !git_available() {
            eprintln!("SKIP: git binary not found");
            return;
        }
        let repo = make_repo(false).expect("make repo");
        let (_d, cache) = temp_cache();
        let sha = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Tag("v1".to_string()))
            .expect("resolve tag");
        let checkout = cache
            .ensure_checkout("pkg", &repo.url, &sha, &CheckoutOpts::default())
            .expect("checkout");
        assert!(
            checkout.join("a.txt").is_file(),
            "tracked file materialized"
        );
        assert!(
            cache.worktree_clean(&checkout).expect("status"),
            "worktree must match HEAD"
        );
    }

    #[test]
    fn corrupt_checkout_heals_on_reuse() {
        if !git_available() {
            eprintln!("SKIP: git binary not found");
            return;
        }
        let repo = make_repo(false).expect("make repo");
        let (_d, cache) = temp_cache();
        let sha = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Tag("v1".to_string()))
            .expect("resolve tag");
        let checkout = cache
            .ensure_checkout("pkg", &repo.url, &sha, &CheckoutOpts::default())
            .expect("checkout");
        assert!(checkout.join("a.txt").is_file());
        // Simulate corruption/tampering: delete a tracked file.
        std::fs::remove_file(checkout.join("a.txt")).expect("delete");
        assert!(!cache.worktree_clean(&checkout).expect("status"));
        // Re-ensuring heals instead of reusing the corrupt tree.
        let healed = cache
            .ensure_checkout("pkg", &repo.url, &sha, &CheckoutOpts::default())
            .expect("heal");
        assert_eq!(healed, checkout);
        assert!(checkout.join("a.txt").is_file(), "file restored");
        assert!(
            cache.worktree_clean(&checkout).expect("status"),
            "worktree clean after heal"
        );
    }

    #[test]
    fn clone_writable_copies_and_checks_out_branch() {
        if !git_available() {
            eprintln!("SKIP: git binary not found");
            return;
        }
        let repo = make_repo(false).expect("make repo");
        let (_d, cache) = temp_cache();
        let sha = cache
            .resolve_ref("pkg", &repo.url, &GitRef::Tag("v1".to_string()))
            .expect("resolve tag");
        let checkout = cache
            .ensure_checkout("pkg", &repo.url, &sha, &CheckoutOpts::default())
            .expect("checkout");
        let branch = cache
            .run_git(&["branch", "--show-current"], Some(&checkout))
            .unwrap_or_default();
        // Detached by default (no branch arg).
        let dst = _d.path().join("dev-pkg");
        cache
            .clone_writable(&checkout, &dst, None)
            .expect("writable clone");
        assert!(dst.join("a.txt").is_file());
        assert!(dst.join(".git").is_dir());
        // Existing destination is an error.
        let err = cache
            .clone_writable(&checkout, &dst, None)
            .expect_err("reuse must fail");
        assert!(err.to_string().contains("already exists"), "got: {err}");
        // Named branch checks out (empty when detached-origin default).
        if !branch.is_empty() {
            let dst2 = _d.path().join("dev-pkg2");
            cache
                .clone_writable(&checkout, &dst2, Some(&branch))
                .expect("branch clone");
            let cur = cache
                .run_git(&["branch", "--show-current"], Some(&dst2))
                .unwrap_or_default();
            assert_eq!(cur, branch);
        }
    }
}
