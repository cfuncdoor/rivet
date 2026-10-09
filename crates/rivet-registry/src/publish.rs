//! Publish uploads: request construction (unit-testable, no network) plus
//! the live `PUT` with bearer auth.
//!
//! Wire format (the server arrives later):
//! `PUT <registry-url>/publish/<name>/<version>`, body = gzip tarball,
//! `Content-Type: application/gzip`, `Authorization: Bearer <token>`,
//! `X-Rivet-Checksum: sha256:…`, optional `X-Rivet-Description`, optional
//! `X-Rivet-Deps` (JSON array of `{"name","req","optional"}`).

use crate::{Error, PackagedTarball, PublishMeta, Result};

/// A constructed publish request: URL + headers; the body is
/// [`PackagedTarball::bytes`]. Kept separate from [`publish`] so tests and
/// the CLI can inspect/log it without performing an upload.
#[derive(Debug, Clone)]
pub struct PublishRequest {
    /// Full upload URL.
    pub url: String,
    /// HTTP method (`PUT`).
    pub method: String,
    /// Headers to send (includes the real `Authorization` value).
    pub headers: Vec<(String, String)>,
    /// Tarball body length in bytes.
    pub body_len: usize,
    /// Tarball checksum (`sha256:…`).
    pub checksum: String,
}

impl PublishRequest {
    /// Headers safe to log: the bearer token is masked.
    pub fn redacted_headers(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .map(|(k, v)| {
                if k.eq_ignore_ascii_case("authorization") {
                    (k.clone(), "Bearer <redacted>".to_string())
                } else {
                    (k.clone(), v.clone())
                }
            })
            .collect()
    }
}

/// Build the publish request without sending it. Errors actionably when
/// `token` is missing (`None`/empty → run `rivet login` first) or the
/// registry URL is not `https://` (publishing to `file://` or plain `http://`
/// is rejected — tokens must never travel in the clear or to disk).
pub fn build_publish_request(
    registry_url: &str,
    meta: &PublishMeta,
    packaged: &PackagedTarball,
    token: Option<&str>,
) -> Result<PublishRequest, Error> {
    let token = token.unwrap_or("").trim();
    if token.is_empty() {
        return Err(Error::AuthMissing {
            registry: registry_url.to_string(),
        });
    }
    let base = crate::normalize_registry_url(registry_url);
    if !base.starts_with("https://") {
        return Err(Error::BadUrl {
            url: registry_url.to_string(),
            reason: "publishing requires an `https://` registry URL".to_string(),
            hint: "publish to the real registry; test packaging offline instead".to_string(),
        });
    }
    if !crate::is_valid_package_name(&meta.name) {
        return Err(Error::BadUrl {
            url: registry_url.to_string(),
            reason: format!("invalid package name `{}`", meta.name),
            hint: "package names use only [a-zA-Z0-9_-]".to_string(),
        });
    }
    if packaged.name != meta.name || packaged.version != meta.version {
        return Err(Error::BadUrl {
            url: registry_url.to_string(),
            reason: format!(
                "packaged tarball is {} v{} but metadata says {} v{}",
                packaged.name, packaged.version, meta.name, meta.version
            ),
            hint: "package and publish the same name/version".to_string(),
        });
    }
    let url = format!("{base}/publish/{}/{}", meta.name, meta.version);
    let mut headers = vec![
        ("Authorization".to_string(), format!("Bearer {token}")),
        ("Content-Type".to_string(), "application/gzip".to_string()),
        ("X-Rivet-Checksum".to_string(), packaged.checksum.clone()),
    ];
    if let Some(description) = &meta.description {
        headers.push(("X-Rivet-Description".to_string(), description.clone()));
    }
    if !meta.deps.is_empty() {
        let deps_json = serde_json::to_string(&meta.deps).map_err(|e| Error::BadUrl {
            url: registry_url.to_string(),
            reason: format!("cannot serialize dep metadata: {e}"),
            hint: "report a bug".to_string(),
        })?;
        headers.push(("X-Rivet-Deps".to_string(), deps_json));
    }
    Ok(PublishRequest {
        url,
        method: "PUT".to_string(),
        headers,
        body_len: packaged.bytes.len(),
        checksum: packaged.checksum.clone(),
    })
}

/// Publish a packaged tarball (live `PUT` with bearer auth).
///
/// `offline` refuses before touching the network. HTTP failures map to
/// actionable errors (401/403 → bad token, 409 → version exists, else
/// status + body snippet).
pub fn publish(
    registry_url: &str,
    packaged: &PackagedTarball,
    meta: &PublishMeta,
    token: Option<&str>,
    offline: bool,
) -> Result<(), Error> {
    if offline {
        return Err(Error::Offline {
            action: format!("publish {} v{}", meta.name, meta.version),
            where_: registry_url.to_string(),
        });
    }
    let request = build_publish_request(registry_url, meta, packaged, token)?;
    let response = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .put(&request.url)
        .set("Content-Type", "application/gzip")
        .set(
            "Authorization",
            &request
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default(),
        )
        .set("X-Rivet-Checksum", &request.checksum)
        .send_bytes(&packaged.bytes);
    match response {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            let body = if body.len() <= 500 {
                body
            } else {
                format!("{}…[truncated]", &body[..500])
            };
            let hint = match code {
                401 | 403 => format!("check the token (`rivet login --registry {registry_url}`)"),
                409 => "that version already exists; bump the version to publish again".to_string(),
                _ => "check the registry URL and try again".to_string(),
            };
            Err(Error::Publish {
                registry: registry_url.to_string(),
                status: code,
                message: body,
                hint,
            })
        }
        Err(other) => Err(Error::Transport {
            action: format!("publish {} v{}", meta.name, meta.version),
            url: request.url.clone(),
            message: other.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IndexDep;

    fn sample() -> (PublishMeta, PackagedTarball) {
        let meta = PublishMeta {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            description: Some("Fast vectors".to_string()),
            deps: vec![IndexDep {
                name: "bar".to_string(),
                req: "^1".to_string(),
                optional: false,
            }],
        };
        let packaged = PackagedTarball {
            name: "foo".to_string(),
            version: "1.0.0".to_string(),
            bytes: b"fake-tarball".to_vec(),
            checksum: crate::sha256_checksum(b"fake-tarball"),
        };
        (meta, packaged)
    }

    #[test]
    fn request_construction() {
        let (meta, packaged) = sample();
        let req = build_publish_request(
            "https://example.com/rivet/",
            &meta,
            &packaged,
            Some("tok123"),
        )
        .expect("request");
        assert_eq!(req.url, "https://example.com/rivet/publish/foo/1.0.0");
        assert_eq!(req.method, "PUT");
        assert_eq!(req.body_len, packaged.bytes.len());
        let auth = req
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .expect("auth header");
        assert_eq!(auth.1, "Bearer tok123");
        // Redaction hides the token.
        let redacted = req.redacted_headers();
        let rauth = redacted
            .iter()
            .find(|(k, _)| k == "Authorization")
            .expect("auth header");
        assert!(!rauth.1.contains("tok123"), "token leaked: {rauth:?}");
        assert!(req.headers.iter().any(|(k, _)| k == "X-Rivet-Checksum"));
        assert!(req.headers.iter().any(|(k, _)| k == "X-Rivet-Description"));
        assert!(req.headers.iter().any(|(k, _)| k == "X-Rivet-Deps"));
    }

    #[test]
    fn request_needs_auth() {
        let (meta, packaged) = sample();
        for token in [None, Some(""), Some("   ")] {
            let err = build_publish_request("https://example.com/r", &meta, &packaged, token)
                .expect_err("auth");
            assert!(matches!(err, Error::AuthMissing { .. }), "got: {err}");
            assert!(err.to_string().contains("rivet login"));
        }
    }

    #[test]
    fn request_rejects_insecure_registry() {
        let (meta, packaged) = sample();
        for url in ["http://example.com/r", "file:///tmp/r"] {
            let err =
                build_publish_request(url, &meta, &packaged, Some("tok")).expect_err("insecure");
            assert!(matches!(err, Error::BadUrl { .. }), "got: {err}");
        }
    }

    #[test]
    fn request_rejects_name_version_mismatch() {
        let (meta, mut packaged) = sample();
        packaged.version = "2.0.0".to_string();
        let err = build_publish_request("https://example.com/r", &meta, &packaged, Some("tok"))
            .expect_err("mismatch");
        assert!(matches!(err, Error::BadUrl { .. }), "got: {err}");
    }

    #[test]
    fn publish_offline_errors_without_network() {
        let (meta, packaged) = sample();
        let err = publish("https://example.com/r", &packaged, &meta, Some("tok"), true)
            .expect_err("offline");
        assert!(matches!(err, Error::Offline { .. }), "got: {err}");
    }
}
