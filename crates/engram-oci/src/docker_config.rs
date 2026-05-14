//! `DockerConfigResolver` — reads `~/.docker/config.json` for OCI
//! registry credentials.
//!
//! Standard docker config format (what `docker login` writes):
//!
//! ```json
//! {
//!   "auths": {
//!     "ghcr.io": {
//!       "auth": "base64(username:password)"
//!     }
//!   }
//! }
//! ```
//!
//! This is the same file every other OCI tool reads (crane, oras,
//! podman, skopeo). Setting up auth becomes "run `docker login`
//! upstream of whatever invokes us" — no engram-specific
//! credential plumbing required.
//!
//! Out of scope for v1: `credHelpers` / `credsStore` entries (those
//! defer auth to an external `docker-credential-*` binary). They're
//! the secure default on some platforms (e.g. macOS Keychain) but
//! shelling to them is a separate workstream — for CI the inline
//! `auth` form is what gets written by `docker login` with a token,
//! which is the common case.

use std::path::PathBuf;

use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;

use crate::{BasicCreds, OciError, RegistryAuthResolver};

/// Reads `${DOCKER_CONFIG}/config.json` (or `~/.docker/config.json`)
/// to resolve credentials for a registry host.
///
/// Missing file, missing entry, or any kind of "no creds here for
/// this host" condition resolves to `Ok(None)` — the OciClient will
/// fall back to anonymous, matching the existing trait contract.
pub struct DockerConfigResolver {
    /// Override path for tests / non-standard layouts. `None` means
    /// look up via env (`DOCKER_CONFIG`) then `$HOME/.docker/config.json`.
    config_path: Option<PathBuf>,
}

impl DockerConfigResolver {
    pub fn new() -> Self {
        Self { config_path: None }
    }

    /// Explicit path, primarily for tests.
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            config_path: Some(path),
        }
    }

    fn resolved_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.config_path {
            return Some(p.clone());
        }
        if let Ok(dir) = std::env::var("DOCKER_CONFIG") {
            return Some(PathBuf::from(dir).join("config.json"));
        }
        std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".docker").join("config.json"))
    }
}

impl Default for DockerConfigResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: std::collections::HashMap<String, AuthEntry>,
}

#[derive(Deserialize)]
struct AuthEntry {
    /// Inline credentials. base64-encoded `username:password`.
    /// `docker login` writes this when no helper is configured.
    #[serde(default)]
    auth: Option<String>,
}

#[async_trait]
impl RegistryAuthResolver for DockerConfigResolver {
    async fn resolve(&self, registry_host: &str) -> Result<Option<BasicCreds>, OciError> {
        let Some(path) = self.resolved_path() else {
            return Ok(None);
        };

        // Missing file = "no docker login ever happened on this
        // machine" = fall back to anonymous. Same as if no entry
        // existed for this host.
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(OciError::from(e)),
        };

        let config: DockerConfig = serde_json::from_slice(&bytes).map_err(|e| {
            OciError::from(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("parse {}: {e}", path.display()),
            ))
        })?;

        // docker writes keys as bare hostnames (`ghcr.io`) by default
        // but legacy/CLI variations also write `https://ghcr.io/v1/`,
        // `https://ghcr.io/`, etc. Try the common variants in order.
        let candidates = [
            registry_host.to_string(),
            format!("https://{registry_host}/"),
            format!("https://{registry_host}"),
            format!("https://{registry_host}/v1/"),
            format!("https://{registry_host}/v2/"),
        ];

        let entry = candidates
            .iter()
            .find_map(|k| config.auths.get(k.as_str()));

        let Some(entry) = entry else {
            return Ok(None);
        };
        let Some(auth_b64) = entry.auth.as_deref() else {
            // Entry exists but has no inline `auth` field — likely
            // configured via credHelpers/credsStore, which we don't
            // support yet. Fall back to anonymous; the operator can
            // export `DOCKER_AUTH_CONFIG` with inline creds or set
            // up creds via `docker login` (which writes inline auth).
            return Ok(None);
        };

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(auth_b64.trim())
            .map_err(|e| {
                OciError::from(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("decode auth for {registry_host}: {e}"),
                ))
            })?;
        let s = std::str::from_utf8(&decoded).map_err(|e| {
            OciError::from(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("auth utf8 for {registry_host}: {e}"),
            ))
        })?;
        let (user, pass) = s.split_once(':').ok_or_else(|| {
            OciError::from(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("auth for {registry_host} not in user:password form"),
            ))
        })?;

        Ok(Some(BasicCreds {
            username: user.to_string(),
            password: pass.to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &tempfile::TempDir, json: &str) -> PathBuf {
        let p = dir.path().join("config.json");
        std::fs::write(&p, json).unwrap();
        p
    }

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    #[tokio::test]
    async fn resolves_credentials_for_matching_host() {
        let tmp = tempfile::tempdir().unwrap();
        let json = format!(
            r#"{{"auths":{{"ghcr.io":{{"auth":"{}"}}}}}}"#,
            b64("alice:s3cret")
        );
        let path = write_config(&tmp, &json);

        let r = DockerConfigResolver::with_path(path);
        let creds = r.resolve("ghcr.io").await.unwrap().unwrap();
        assert_eq!(creds.username, "alice");
        assert_eq!(creds.password, "s3cret");
    }

    #[tokio::test]
    async fn returns_none_for_unmatched_host() {
        let tmp = tempfile::tempdir().unwrap();
        let json = format!(
            r#"{{"auths":{{"ghcr.io":{{"auth":"{}"}}}}}}"#,
            b64("alice:s3cret")
        );
        let path = write_config(&tmp, &json);

        let r = DockerConfigResolver::with_path(path);
        assert!(r.resolve("docker.io").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn returns_none_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");

        let r = DockerConfigResolver::with_path(path);
        assert!(r.resolve("ghcr.io").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn resolves_legacy_https_url_key() {
        // Older docker CLIs wrote keys as `https://<host>/v1/`.
        let tmp = tempfile::tempdir().unwrap();
        let json = format!(
            r#"{{"auths":{{"https://ghcr.io/v1/":{{"auth":"{}"}}}}}}"#,
            b64("alice:s3cret")
        );
        let path = write_config(&tmp, &json);

        let r = DockerConfigResolver::with_path(path);
        let creds = r.resolve("ghcr.io").await.unwrap().unwrap();
        assert_eq!(creds.username, "alice");
    }

    #[tokio::test]
    async fn returns_none_for_credhelper_only_entry() {
        // Some docker configs have entries with no inline `auth`,
        // deferring to a credential helper (`credHelpers`). v1 of
        // this resolver doesn't shell to helpers; fall back to
        // anonymous so the caller picks up the slack (e.g. by
        // setting CI auth via `docker login` with --password).
        let tmp = tempfile::tempdir().unwrap();
        let json = r#"{"auths":{"ghcr.io":{}},"credHelpers":{"ghcr.io":"gh"}}"#;
        let path = write_config(&tmp, json);

        let r = DockerConfigResolver::with_path(path);
        assert!(r.resolve("ghcr.io").await.unwrap().is_none());
    }
}
