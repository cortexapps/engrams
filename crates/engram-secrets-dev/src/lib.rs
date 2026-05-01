//! Development [`SecretStore`] implementations.
//!
//! Three flavors, all suitable only for non-production use:
//!
//! - [`InMemorySecretStore`] — fixed map. Tests, demos.
//! - [`EnvSecretStore`] — reads `$NAME` from the host environment.
//!   Lowest-friction dev setup: `GITHUB_TOKEN=... just dev` and the
//!   sandbox's `GITHUB_TOKEN` resolves to the same value.
//! - [`DotenvSecretStore`] — reads from a `.env`-style file. For
//!   keeping dev secrets out of the shell history without standing
//!   up a real backend.
//!
//! All three honor the `SecretSchema.required` flag (via the default
//! `resolve` impl on the trait) and ignore `schema.ref` (since dev
//! deployments use `name`-based lookup). A production deployment
//! configures `engram-secrets-gcp` (or vault/aws/k8s) and gets ref
//! parsing for free.
//!
//! # Safety
//!
//! Real secrets transit env vars / disk in plaintext here. That's
//! fine for `your_org/dev_user` against your own GitHub PAT; it's not
//! ok for production. Production deployments must configure a
//! `SecretMode::Broker`-compatible backend (placeholder substitution
//! through a network proxy).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use engram_core::traits::{SecretContext, SecretStore};
use engram_core::types::SecretSchema;
use engram_core::SecretError;
use parking_lot::RwLock;

// ---------------------------------------------------------------------
// InMemorySecretStore
// ---------------------------------------------------------------------

#[derive(Default)]
pub struct InMemorySecretStore {
    secrets: RwLock<HashMap<String, String>>,
}

impl InMemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_secrets<I, K, V>(secrets: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let map: HashMap<String, String> = secrets
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        Self {
            secrets: RwLock::new(map),
        }
    }

    pub fn insert(&self, name: impl Into<String>, value: impl Into<String>) {
        self.secrets.write().insert(name.into(), value.into());
    }

    pub fn remove(&self, name: &str) {
        self.secrets.write().remove(name);
    }

    pub fn arc(self) -> Arc<dyn SecretStore> {
        Arc::new(self)
    }
}

#[async_trait]
impl SecretStore for InMemorySecretStore {
    async fn get(
        &self,
        _ctx: &SecretContext<'_>,
        name: &str,
        _schema: &SecretSchema,
    ) -> Result<Option<String>, SecretError> {
        Ok(self.secrets.read().get(name).cloned())
    }
}

// ---------------------------------------------------------------------
// EnvSecretStore
// ---------------------------------------------------------------------

/// Custom env-var resolver. Default is `std::env::var`; tests and
/// CLI overlays inject their own.
type EnvGetter = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Reads from process environment. Each lookup snapshots `std::env`
/// at call time so a `cargo run` with `GITHUB_TOKEN=...` Just Works.
///
/// Optional prefix lets you scope: with `prefix = "ENGRAM_"`, the
/// manifest's `GITHUB_TOKEN` looks up `$ENGRAM_GITHUB_TOKEN`. Avoids
/// collisions with the host env when running multiple coordinators.
///
/// Tests inject a custom getter via [`EnvSecretStore::with_getter`]
/// so they don't have to mutate the global `std::env` (which is racy
/// across threads and forbidden under our `forbid(unsafe_code)` lint).
pub struct EnvSecretStore {
    prefix: Option<String>,
    getter: EnvGetter,
}

impl EnvSecretStore {
    pub fn new() -> Self {
        Self {
            prefix: None,
            getter: Box::new(|k| std::env::var(k).ok()),
        }
    }

    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        Self {
            prefix: Some(prefix.into()),
            ..Self::new()
        }
    }

    /// Custom getter — used by tests, and by callers that want to
    /// layer a secrets overlay (e.g. a CLI-scoped map on top of the
    /// process env).
    pub fn with_getter<F>(getter: F) -> Self
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        Self {
            prefix: None,
            getter: Box::new(getter),
        }
    }
}

impl Default for EnvSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretStore for EnvSecretStore {
    async fn get(
        &self,
        _ctx: &SecretContext<'_>,
        name: &str,
        _schema: &SecretSchema,
    ) -> Result<Option<String>, SecretError> {
        let key = match &self.prefix {
            Some(p) => format!("{p}{name}"),
            None => name.to_string(),
        };
        Ok((self.getter)(&key))
    }
}

// ---------------------------------------------------------------------
// DotenvSecretStore
// ---------------------------------------------------------------------

/// Reads from a `.env`-style file: `NAME=VALUE` per line, `#` for
/// comments, optional double-quoted values. Re-reads on every lookup
/// so editing the file is picked up live (file is small, devs only).
pub struct DotenvSecretStore {
    path: PathBuf,
}

impl DotenvSecretStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl SecretStore for DotenvSecretStore {
    async fn get(
        &self,
        _ctx: &SecretContext<'_>,
        name: &str,
        _schema: &SecretSchema,
    ) -> Result<Option<String>, SecretError> {
        let bytes = match tokio::fs::read(&self.path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(SecretError::Backend(
                    format!("read {}: {e}", self.path.display()).into(),
                ));
            }
        };
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| SecretError::BadValue(format!("{} is not UTF-8", self.path.display())))?;
        Ok(parse_dotenv_lookup(text, name))
    }
}

fn parse_dotenv_lookup(content: &str, target: &str) -> Option<String> {
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, val) = match line.split_once('=') {
            Some(p) => p,
            None => continue,
        };
        if key.trim() != target {
            continue;
        }
        let val = val.trim();
        // Strip matched single or double quotes if both ends match.
        let value = if (val.starts_with('"') && val.ends_with('"') && val.len() >= 2)
            || (val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2)
        {
            val[1..val.len() - 1].to_string()
        } else {
            val.to_string()
        };
        return Some(value);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> SecretContext<'static> {
        SecretContext {
            repo: "cortex/api",
            image_tag: "warm-1",
        }
    }

    fn schema(required: bool) -> SecretSchema {
        SecretSchema {
            required,
            ..SecretSchema::default()
        }
    }

    #[tokio::test]
    async fn in_memory_store_returns_known_secrets_and_misses_others() {
        let store = InMemorySecretStore::with_secrets([("GITHUB_TOKEN", "ghp_test")]);
        assert_eq!(
            store
                .get(&ctx(), "GITHUB_TOKEN", &schema(true))
                .await
                .unwrap(),
            Some("ghp_test".into()),
        );
        assert_eq!(
            store.get(&ctx(), "MISSING", &schema(false)).await.unwrap(),
            None,
        );
    }

    #[tokio::test]
    async fn in_memory_resolve_errors_on_missing_required_secret() {
        let store = InMemorySecretStore::with_secrets([("PRESENT", "1")]);
        let mut schema_map = HashMap::new();
        schema_map.insert("PRESENT".into(), schema(true));
        schema_map.insert("MISSING".into(), schema(true));
        let res = store.resolve(&ctx(), &schema_map, None).await;
        match res {
            Err(SecretError::Backend(e)) => assert!(e.to_string().contains("MISSING")),
            other => panic!("expected Backend(MISSING), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn in_memory_resolve_skips_optional_misses() {
        let store = InMemorySecretStore::with_secrets([("PRESENT", "1")]);
        let mut schema_map = HashMap::new();
        schema_map.insert("PRESENT".into(), schema(true));
        schema_map.insert("MISSING".into(), schema(false));
        let bundle = store.resolve(&ctx(), &schema_map, None).await.unwrap();
        assert!(bundle.secrets.contains_key("PRESENT"));
        assert!(!bundle.secrets.contains_key("MISSING"));
    }

    #[tokio::test]
    async fn env_store_resolves_via_injected_getter() {
        // Use the injectable getter so we don't mutate `std::env`
        // (forbidden under `forbid(unsafe_code)` in 2024 edition).
        // The default getter just calls `std::env::var` — see
        // `EnvSecretStore::new`.
        let store = EnvSecretStore::with_getter(|k| match k {
            "GITHUB_TOKEN" => Some("ghp_from_env".into()),
            _ => None,
        });
        assert_eq!(
            store
                .get(&ctx(), "GITHUB_TOKEN", &schema(true))
                .await
                .unwrap(),
            Some("ghp_from_env".into()),
        );
        assert_eq!(
            store
                .get(&ctx(), "DEFINITELY_NOT_SET", &schema(false))
                .await
                .unwrap(),
            None,
        );
    }

    #[tokio::test]
    async fn env_store_with_prefix_namespaces_lookups() {
        // Capture the resolved key the getter sees, to assert that
        // `prefix + name` is what gets looked up.
        use std::sync::Mutex;
        let observed: std::sync::Arc<Mutex<Vec<String>>> = std::sync::Arc::new(Mutex::new(vec![]));
        let observed_clone = observed.clone();
        let store = {
            let mut s = EnvSecretStore::with_getter(move |k| {
                observed_clone.lock().unwrap().push(k.to_string());
                if k == "ENGRAM_NS_PREFIXED_FOO" {
                    Some("scoped".into())
                } else {
                    None
                }
            });
            s.prefix = Some("ENGRAM_NS_".into());
            s
        };
        let value = store
            .get(&ctx(), "PREFIXED_FOO", &schema(true))
            .await
            .unwrap();
        assert_eq!(value, Some("scoped".into()));
        assert_eq!(
            observed.lock().unwrap().as_slice(),
            &["ENGRAM_NS_PREFIXED_FOO".to_string()],
            "prefix must be applied to the lookup key",
        );
    }

    #[tokio::test]
    async fn dotenv_store_parses_simple_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        tokio::fs::write(
            &path,
            b"# comment\nGITHUB_TOKEN=ghp_dotenv\nempty=\nexport SCOPED=yes\n",
        )
        .await
        .unwrap();
        let store = DotenvSecretStore::new(&path);
        assert_eq!(
            store
                .get(&ctx(), "GITHUB_TOKEN", &schema(true))
                .await
                .unwrap(),
            Some("ghp_dotenv".into()),
        );
        assert_eq!(
            store.get(&ctx(), "empty", &schema(false)).await.unwrap(),
            Some("".into()),
        );
        // `export FOO=bar` shell-style lines also resolve.
        assert_eq!(
            store.get(&ctx(), "SCOPED", &schema(true)).await.unwrap(),
            Some("yes".into()),
        );
    }

    #[tokio::test]
    async fn dotenv_store_handles_quoted_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        tokio::fs::write(
            &path,
            b"DOUBLE=\"with spaces\"\nSINGLE='ssh-keygen -t rsa'\n",
        )
        .await
        .unwrap();
        let store = DotenvSecretStore::new(&path);
        assert_eq!(
            store.get(&ctx(), "DOUBLE", &schema(true)).await.unwrap(),
            Some("with spaces".into()),
        );
        assert_eq!(
            store.get(&ctx(), "SINGLE", &schema(true)).await.unwrap(),
            Some("ssh-keygen -t rsa".into()),
        );
    }

    #[tokio::test]
    async fn dotenv_store_returns_none_when_file_missing() {
        let store = DotenvSecretStore::new("/no/such/path/.env");
        assert_eq!(
            store.get(&ctx(), "ANY", &schema(false)).await.unwrap(),
            None,
        );
    }
}
