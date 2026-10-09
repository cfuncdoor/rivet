//! Registry login tokens: a JSON object in `~/.rivet/credentials` mapping
//! registry URL → token.
//!
//! The CLI passes the home directory explicitly (`home` param) so tests use
//! a temp dir and production passes the real home. Unix files are chmodded
//! `0600` best-effort (a chmod failure warns, never fails the login);
//! Windows has no std chmod equivalent, so login warns there instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Path of the credential store under `home` (`~/.rivet/credentials`).
pub fn credentials_path(home: &Path) -> PathBuf {
    home.join(".rivet").join("credentials")
}

/// Store `token` for `registry_url` (creates parent dirs; overwrites any
/// previous token for the same URL). Returns the store path.
pub fn login(registry_url: &str, token: &str, home: &Path) -> Result<PathBuf, Error> {
    if registry_url.trim().is_empty() {
        return Err(Error::Login {
            message: "registry URL is empty".to_string(),
            hint: "pass `--registry https://…`".to_string(),
        });
    }
    if token.trim().is_empty() {
        return Err(Error::Login {
            message: "token is empty".to_string(),
            hint: "paste a non-empty registry token".to_string(),
        });
    }
    let path = credentials_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::io("create credential dir", parent, e))?;
    }
    let mut map = read_store(&path)?;
    map.insert(registry_url.to_string(), token.to_string());
    write_store(&path, &map)?;
    restrict_permissions(&path);
    Ok(path)
}

/// Load the stored token for `registry_url`, if any. A missing store file
/// is `Ok(None)` (never logged in); corrupt JSON is an error.
pub fn load_token(registry_url: &str, home: &Path) -> Result<Option<String>, Error> {
    let path = credentials_path(home);
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io("read credential store", &path, e)),
        Ok(text) => {
            let map: BTreeMap<String, String> =
                serde_json::from_str(&text).map_err(|e| Error::Login {
                    message: format!("credential store `{}` is corrupt: {e}", path.display()),
                    hint: "delete the file and run `rivet login` again".to_string(),
                })?;
            Ok(map.get(registry_url).cloned())
        }
    }
}

/// Forget the token for `registry_url`. Returns `true` when a token was
/// removed.
pub fn logout(registry_url: &str, home: &Path) -> Result<bool, Error> {
    let path = credentials_path(home);
    let mut map = read_store(&path)?;
    let removed = map.remove(registry_url).is_some();
    if removed {
        write_store(&path, &map)?;
        restrict_permissions(&path);
    }
    Ok(removed)
}

fn read_store(path: &Path) -> Result<BTreeMap<String, String>, Error> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(Error::io("read credential store", path, e)),
        Ok(text) => serde_json::from_str(&text).map_err(|e| Error::Login {
            message: format!("credential store `{}` is corrupt: {e}", path.display()),
            hint: "delete the file and run `rivet login` again".to_string(),
        }),
    }
}

fn write_store(path: &Path, map: &BTreeMap<String, String>) -> Result<(), Error> {
    let mut text = serde_json::to_string_pretty(map).map_err(|e| Error::Login {
        message: format!("cannot serialize credential store: {e}"),
        hint: "report a bug".to_string(),
    })?;
    text.push('\n');
    std::fs::write(path, text).map_err(|e| Error::io("write credential store", path, e))
}

/// Best-effort `0600` on Unix; warn on Windows (no std ACL API there) and on
/// chmod failure (never fail the login over permissions).
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let perms = std::fs::Permissions::from_mode(0o600);
        if let Err(e) = std::fs::set_permissions(path, perms) {
            eprintln!(
                "warning: could not restrict permissions on `{}`: {e}",
                path.display()
            );
        }
    }
    #[cfg(windows)]
    {
        eprintln!(
            "warning: credential store `{}` inherits directory ACLs; restrict them manually",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_roundtrip_and_logout() {
        let home = tempfile::TempDir::new().expect("tempdir");
        assert_eq!(
            load_token("https://example.com/r", home.path()).expect("load"),
            None
        );
        let path = login("https://example.com/r", "secret-token", home.path()).expect("login");
        assert_eq!(path, credentials_path(home.path()));
        assert_eq!(
            load_token("https://example.com/r", home.path()).expect("load"),
            Some("secret-token".to_string())
        );
        // Second registry coexists.
        login("https://other.example/x", "tok2", home.path()).expect("login2");
        assert_eq!(
            load_token("https://other.example/x", home.path()).expect("load"),
            Some("tok2".to_string())
        );
        assert!(logout("https://example.com/r", home.path()).expect("logout"));
        assert_eq!(
            load_token("https://example.com/r", home.path()).expect("load"),
            None
        );
        assert!(!logout("https://example.com/r", home.path()).expect("logout2"));
    }

    #[test]
    fn login_rejects_empty() {
        let home = tempfile::TempDir::new().expect("tempdir");
        assert!(matches!(
            login("", "tok", home.path()),
            Err(Error::Login { .. })
        ));
        assert!(matches!(
            login("https://example.com/r", "  ", home.path()),
            Err(Error::Login { .. })
        ));
    }

    #[test]
    fn corrupt_store_errors_actionably() {
        let home = tempfile::TempDir::new().expect("tempdir");
        let path = credentials_path(home.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "not json").expect("write");
        let err = load_token("https://example.com/r", home.path()).expect_err("corrupt");
        assert!(matches!(err, Error::Login { .. }), "got: {err}");
        assert!(err.to_string().contains("rivet login"));
    }

    #[cfg(unix)]
    #[test]
    fn store_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = tempfile::TempDir::new().expect("tempdir");
        let path = login("https://example.com/r", "tok", home.path()).expect("login");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {mode:o}");
    }
}
