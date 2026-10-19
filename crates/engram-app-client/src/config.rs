//! Credential + host resolution (the `gh` model).
//!
//! Precedence for the API key:
//!   1. `ENGRAMS_API_KEY` (CI, scripts) — never persisted.
//!   2. The stored entry for the active host in
//!      `$XDG_CONFIG_HOME|~/.config/engrams/hosts.json` (0600; written by
//!      `engrams auth login`).
//!   3. None → commands that need auth fail with a pointer to `auth login`.
//!
//! The active host URL: `--url` > `ENGRAMS_URL` > [`DEFAULT_HOST`].
//! `hosts.json` is keyed by normalized host URL so one machine can hold
//! credentials for prod and a local dev stack side by side.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const DEFAULT_HOST: &str = "https://engrams.cortex.io";

/// The one OAuth client_id the orchestrator's device flow accepts. Must stay
/// in lockstep with `DEVICE_CLIENT_ID` in `orchestrator/src/auth/better-auth.ts`.
pub const DEVICE_CLIENT_ID: &str = "engrams-cli";

/// One stored login. Serialized with the camelCase keys the Bun CLI wrote,
/// so an existing `hosts.json` reads unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostCredential {
    /// The `engk_…` plaintext (only its hash exists server-side).
    pub api_key: String,
    /// The apikey row id — `RevokeCliKey`'s handle at logout.
    pub key_id: String,
    /// Owner email at login time (display only; the server re-resolves).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

type HostsFile = BTreeMap<String, HostCredential>;

pub fn hosts_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"));
    base.join("engrams").join("hosts.json")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Strip trailing slashes so flag/env/stored spellings key identically.
pub fn normalize_host(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

/// `--url` > `ENGRAMS_URL` > the default, normalized. Empty strings count as
/// unset, as they do for the Bun CLI's `||` chain.
pub fn resolve_host(flag_url: Option<&str>) -> String {
    let from_flag = flag_url.filter(|s| !s.is_empty()).map(str::to_string);
    let from_env = std::env::var("ENGRAMS_URL").ok().filter(|s| !s.is_empty());
    normalize_host(
        &from_flag
            .or(from_env)
            .unwrap_or_else(|| DEFAULT_HOST.to_string()),
    )
}

/// Unreadable or malformed files read as empty, as in the Bun CLI.
fn read_hosts() -> HostsFile {
    std::fs::read(hosts_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_hosts(hosts: &HostsFile) -> io::Result<()> {
    let path = hosts_path();
    let mut body = serde_json::to_vec_pretty(hosts)?;
    body.push(b'\n');
    std::fs::write(&path, body)?;
    restrict(&path)
}

#[cfg(unix)]
fn restrict(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) -> io::Result<()> {
    Ok(())
}

pub fn stored_credential(host: &str) -> Option<HostCredential> {
    read_hosts().remove(&normalize_host(host))
}

pub fn store_credential(host: &str, cred: HostCredential) -> io::Result<()> {
    let path = hosts_path();
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let mut hosts = read_hosts();
    hosts.insert(normalize_host(host), cred);
    write_hosts(&hosts)
}

#[cfg(unix)]
fn create_private_dir(dir: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn create_private_dir(dir: &std::path::Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Forget the stored login for `host`. A missing entry is not an error.
pub fn delete_credential(host: &str) -> io::Result<()> {
    let mut hosts = read_hosts();
    if hosts.remove(&normalize_host(host)).is_none() {
        return Ok(());
    }
    write_hosts(&hosts)
}

/// The key for a request against `host`, or `None` when logged out.
pub fn resolve_api_key(host: &str) -> Option<String> {
    std::env::var("ENGRAMS_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| stored_credential(host).map(|c| c.api_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_trailing_slashes() {
        assert_eq!(normalize_host("https://x.example/"), "https://x.example");
        assert_eq!(normalize_host("https://x.example//"), "https://x.example");
        assert_eq!(normalize_host("http://flag.example"), "http://flag.example");
    }

    #[test]
    fn credential_json_uses_the_bun_cli_keys() {
        let c = HostCredential {
            api_key: "engk_k".into(),
            key_id: "id-1".into(),
            user: None,
        };
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"apiKey":"engk_k","keyId":"id-1"}"#
        );
        let back: HostCredential =
            serde_json::from_str(r#"{"apiKey":"a","keyId":"b","user":"u@x"}"#).unwrap();
        assert_eq!(back.user.as_deref(), Some("u@x"));
    }
}
