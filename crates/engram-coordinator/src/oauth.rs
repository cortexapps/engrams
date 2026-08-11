//! ADR 0106: provider-neutral OAuth lifecycle and the OpenAI Codex driver.
//!
//! Generic code owns subject scoping, KEK sealing, flow persistence, CAS, and
//! session delivery. Provider code is trusted but narrow: acquire a cache and
//! validate a refreshed cache. No provider message or credential is logged.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use engram_core::traits::{Clock, Entropy, MetadataStore};
use engram_core::types::oauth::{
    NewSealedOAuthCredential, OAuthAccountMetadata, OAuthCredentialKey, OAuthFlow, OAuthFlowStatus,
    OAuthSubjectKind, SealedOAuthCredential,
};
use engram_core::{MetaError, SessionId};
use engram_crypto::{CredCipher, MasterKeyProvider, SealedCred};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, Semaphore};

pub const OPENAI_CODEX_PROVIDER: &str = "openai-codex";
pub const MAX_OAUTH_BUNDLE_BYTES: usize = 256 * 1024;
const FLOW_TTL: chrono::Duration = chrono::Duration::minutes(15);
const FLOW_LEASE: chrono::Duration = chrono::Duration::seconds(30);
const FLOW_LEASE_RENEW: Duration = Duration::from_secs(10);
const TERMINAL_RETENTION: chrono::Duration = chrono::Duration::hours(1);

#[derive(Debug)]
pub struct OAuthDriverError {
    pub code: &'static str,
    /// Sanitized provider-independent detail. Must never contain raw JSON,
    /// device codes, paths containing credentials, or token material.
    pub detail: String,
}

impl OAuthDriverError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

pub struct ValidatedOAuthBundle {
    pub payload: Vec<u8>,
    pub metadata: OAuthAccountMetadata,
    /// Access-token expiry when the driver knows it (redirect drivers over
    /// expiring providers); `None` for opaque device-flow caches.
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[async_trait]
pub trait OAuthFlowHandle: Send {
    fn verification_url(&self) -> &str;
    fn user_code(&self) -> &str;
    async fn complete(&mut self) -> Result<ValidatedOAuthBundle, OAuthDriverError>;
}

#[async_trait]
pub trait OAuthDriver: Send + Sync {
    fn provider(&self) -> &'static str;
    async fn begin(&self) -> Result<Box<dyn OAuthFlowHandle>, OAuthDriverError>;
    async fn validate_cache(
        &self,
        payload: &[u8],
        expected_account_id: &str,
    ) -> Result<ValidatedOAuthBundle, OAuthDriverError>;
}

#[derive(Clone, Debug)]
pub struct BegunOAuthFlow {
    pub flow: OAuthFlow,
    pub verification_url: String,
    pub user_code: String,
}

struct ActiveFlow {
    key: OAuthCredentialKey,
    cancel: parking_lot::Mutex<Option<oneshot::Sender<()>>>,
}

pub struct OAuthManager {
    pub(crate) meta: Arc<dyn MetadataStore>,
    pub(crate) kek: Arc<dyn MasterKeyProvider>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) entropy: Arc<dyn Entropy>,
    /// Org-secret resolution for connector redirect flows (client id/secret
    /// refs). The coordinator is the only tier that reads these.
    pub(crate) secrets: Arc<dyn engram_core::traits::SecretStore>,
    pub(crate) replica: String,
    drivers: BTreeMap<String, Arc<dyn OAuthDriver>>,
    active: DashMap<uuid::Uuid, ActiveFlow>,
    concurrency: Arc<Semaphore>,
    /// Per-key in-process single-flight for connector refresh: a burst of
    /// resolvers refreshes once. Cross-replica dedup is the advisory claim
    /// column; correctness is the version CAS.
    pub(crate) refresh_flights: DashMap<OAuthCredentialKey, Arc<tokio::sync::Mutex<()>>>,
}

impl OAuthManager {
    pub fn new(
        meta: Arc<dyn MetadataStore>,
        kek: Arc<dyn MasterKeyProvider>,
        clock: Arc<dyn Clock>,
        entropy: Arc<dyn Entropy>,
        secrets: Arc<dyn engram_core::traits::SecretStore>,
    ) -> Arc<Self> {
        let codex_bin = resolve_codex_bin();
        let driver: Arc<dyn OAuthDriver> = Arc::new(OpenAiCodexDriver { codex_bin });
        let mut drivers = BTreeMap::new();
        drivers.insert(driver.provider().to_string(), driver);
        let replica = std::env::var("HOSTNAME")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("local-{}", std::process::id()));
        Arc::new(Self {
            meta,
            kek,
            clock,
            entropy,
            secrets,
            replica,
            drivers,
            active: DashMap::new(),
            concurrency: Arc::new(Semaphore::new(16)),
            refresh_flights: DashMap::new(),
        })
    }

    pub async fn begin(
        self: &Arc<Self>,
        key: OAuthCredentialKey,
    ) -> Result<BegunOAuthFlow, OAuthServiceError> {
        validate_key(&key)?;
        let driver = self
            .drivers
            .get(&key.provider)
            .cloned()
            .ok_or_else(|| OAuthServiceError::BadRequest("unknown OAuth provider".into()))?;
        let permit = self
            .concurrency
            .clone()
            .try_acquire_owned()
            .map_err(|_| OAuthServiceError::Busy)?;
        let mut handle = driver.begin().await.map_err(OAuthServiceError::Driver)?;
        let flow_id = self.entropy.uuid();
        let now = self.clock.now_utc();
        let flow = OAuthFlow {
            id: flow_id,
            key: key.clone(),
            owner_replica: self.replica.clone(),
            lease_expires_at: now + FLOW_LEASE,
            expires_at: now + FLOW_TTL,
            status: OAuthFlowStatus::Pending,
            error_code: None,
            created_at: now,
            updated_at: now,
        };
        self.meta.create_oauth_flow(flow.clone()).await?;
        let verification_url = handle.verification_url().to_string();
        let user_code = handle.user_code().to_string();
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        self.active.insert(
            flow_id,
            ActiveFlow {
                key,
                cancel: parking_lot::Mutex::new(Some(cancel_tx)),
            },
        );
        let manager = self.clone();
        let flow_key = flow.key.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut expiry = manager
                .clock
                .sleep(FLOW_TTL.to_std().expect("positive OAuth flow lifetime"));
            let mut completion = Box::pin(handle.complete());
            let outcome = loop {
                let renewal = manager.clock.sleep(FLOW_LEASE_RENEW);
                tokio::select! {
                    result = &mut completion => break match result {
                        Ok(bundle) => match manager.publish_bundle(&flow_key, bundle).await {
                            Ok(()) => (OAuthFlowStatus::Succeeded, None),
                            Err(error) => {
                                tracing::warn!(flow_id = %flow_id, code = error.code(), "OAuth bundle publish failed");
                                (OAuthFlowStatus::Failed, Some(error.code().to_string()))
                            }
                        },
                        Err(error) => {
                            tracing::warn!(flow_id = %flow_id, code = error.code, "OAuth provider flow failed");
                            let status = if error.code == "authorization_denied" {
                                OAuthFlowStatus::Denied
                            } else {
                                OAuthFlowStatus::Failed
                            };
                            (status, Some(error.code.to_string()))
                        }
                    },
                    _ = &mut cancel_rx => break (OAuthFlowStatus::Cancelled, None),
                    _ = &mut expiry => break (OAuthFlowStatus::Expired, Some("flow_expired".into())),
                    _ = renewal => {
                        let lease_expires_at = manager.clock.now_utc() + FLOW_LEASE;
                        if manager.meta.renew_oauth_flow_lease(
                            flow_id,
                            &manager.replica,
                            lease_expires_at,
                        ).await.is_err() {
                            break (OAuthFlowStatus::OwnerLost, Some("owner_lost".into()));
                        }
                    },
                }
            };
            if let Err(error) = manager
                .meta
                .finish_oauth_flow(flow_id, &manager.replica, outcome.0, outcome.1.as_deref())
                .await
            {
                tracing::warn!(flow_id = %flow_id, error = %error, "OAuth terminal status write lost its lease");
            }
            manager.active.remove(&flow_id);
        });
        Ok(BegunOAuthFlow {
            flow,
            verification_url,
            user_code,
        })
    }

    pub async fn get_flow(
        &self,
        subject: &OAuthCredentialKey,
        id: uuid::Uuid,
    ) -> Result<OAuthFlow, OAuthServiceError> {
        let flow = self
            .meta
            .get_oauth_flow(id)
            .await?
            .ok_or(OAuthServiceError::NotFound)?;
        if flow.key.subject_kind != subject.subject_kind
            || flow.key.subject_id != subject.subject_id
        {
            return Err(OAuthServiceError::NotFound);
        }
        Ok(flow)
    }

    /// ADR 0115: fetch a flow row by id with no subject fence. The trusted
    /// orchestrator callback route uses it to learn WHICH subject to
    /// authorize before completing; the row carries no secrets.
    pub async fn lookup_flow(&self, id: uuid::Uuid) -> Result<OAuthFlow, OAuthServiceError> {
        self.meta
            .get_oauth_flow(id)
            .await?
            .ok_or(OAuthServiceError::NotFound)
    }

    pub async fn cancel(
        &self,
        subject: &OAuthCredentialKey,
        id: uuid::Uuid,
    ) -> Result<OAuthFlow, OAuthServiceError> {
        let flow = self.get_flow(subject, id).await?;
        if flow.status.is_terminal() {
            return Ok(flow);
        }
        let Some(active) = self.active.get(&id) else {
            return Err(OAuthServiceError::OwnerLost);
        };
        if active.key.subject_kind != subject.subject_kind
            || active.key.subject_id != subject.subject_id
        {
            return Err(OAuthServiceError::NotFound);
        }
        if let Some(cancel) = active.cancel.lock().take() {
            let _ = cancel.send(());
        }
        drop(active);
        // Persist eagerly so polling observes cancellation without waiting for
        // the subprocess task to schedule. The background write is fenced and
        // harmlessly conflicts.
        self.meta
            .finish_oauth_flow(id, &self.replica, OAuthFlowStatus::Cancelled, None)
            .await?;
        self.get_flow(subject, id).await
    }

    /// `subject_id = None` lists every credential of the kind (the connector
    /// status surface reads all connector credentials in one call).
    pub async fn list(
        &self,
        kind: OAuthSubjectKind,
        subject_id: Option<&str>,
    ) -> Result<Vec<SealedOAuthCredential>, OAuthServiceError> {
        Ok(self.meta.list_oauth_credentials(kind, subject_id).await?)
    }

    pub async fn disconnect(
        &self,
        key: &OAuthCredentialKey,
        expected_version: i64,
    ) -> Result<SealedOAuthCredential, OAuthServiceError> {
        Ok(self
            .meta
            .revoke_oauth_credential(key, expected_version)
            .await?)
    }

    pub async fn fetch_session(
        &self,
        session_id: SessionId,
    ) -> Result<(String, i64, Vec<u8>), OAuthServiceError> {
        let binding = self
            .meta
            .get_session_oauth_binding(session_id)
            .await?
            .ok_or(OAuthServiceError::NotFound)?;
        let row = self
            .meta
            .get_oauth_credential(&binding.key)
            .await?
            .filter(|row| row.revoked_at.is_none())
            .ok_or(OAuthServiceError::Disconnected)?;
        let payload = self.open(&row).await?;
        if payload.len() > MAX_OAUTH_BUNDLE_BYTES {
            return Err(OAuthServiceError::InvalidBundle);
        }
        Ok((binding.key.provider, row.version, payload))
    }

    pub async fn update_session(
        &self,
        session_id: SessionId,
        expected_version: i64,
        payload: &[u8],
    ) -> Result<(String, i64, Vec<u8>), OAuthServiceError> {
        let binding = self
            .meta
            .get_session_oauth_binding(session_id)
            .await?
            .ok_or(OAuthServiceError::NotFound)?;
        let current = self
            .meta
            .get_oauth_credential(&binding.key)
            .await?
            .filter(|row| row.revoked_at.is_none())
            .ok_or(OAuthServiceError::Disconnected)?;
        if current.version != expected_version {
            return Err(OAuthServiceError::Conflict);
        }
        let driver = self
            .drivers
            .get(&binding.key.provider)
            .ok_or_else(|| OAuthServiceError::BadRequest("unknown OAuth provider".into()))?;
        let validated = driver
            .validate_cache(payload, &current.metadata.account_id)
            .await
            .map_err(OAuthServiceError::Driver)?;
        self.publish_bundle_at(&binding.key, validated, Some(expected_version))
            .await?;
        self.fetch_session(session_id).await
    }

    /// Record that the provider terminally rejected this session's harness
    /// credential.
    ///
    /// A harness bundle refreshes in-guest (ADR 0115), so the guest is the
    /// only observer of a refresh that fails for good — a revoked grant, or
    /// a refresh token another client already rotated away. Without this
    /// report the row keeps reading `connected` while every turn 401s, and
    /// the operator sees no reason to reconnect.
    ///
    /// The CAS on `expected_version` keeps a slow reporter from burying a
    /// credential that a racing session has since repaired: if the version
    /// moved, the winner's bundle stands and this call reports success.
    pub async fn report_session_broken(
        &self,
        session_id: SessionId,
        expected_version: i64,
        reason: &str,
    ) -> Result<(), OAuthServiceError> {
        let binding = self
            .meta
            .get_session_oauth_binding(session_id)
            .await?
            .ok_or(OAuthServiceError::NotFound)?;
        match self
            .meta
            .mark_oauth_credential_broken(&binding.key, expected_version, reason)
            .await
        {
            Ok(_) => Ok(()),
            // The version moved under us: a racer refreshed successfully, so
            // the credential is healthy and must NOT be marked broken.
            Err(MetaError::Conflict(_)) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn run_cleanup_once(&self) -> Result<u64, MetaError> {
        let now = self.clock.now_utc();
        self.meta
            .cleanup_oauth_flows(now, now - TERMINAL_RETENTION)
            .await
    }

    pub(crate) async fn publish_bundle(
        &self,
        key: &OAuthCredentialKey,
        bundle: ValidatedOAuthBundle,
    ) -> Result<(), OAuthServiceError> {
        let current = self.meta.get_oauth_credential(key).await?;
        if let Some(current) = &current {
            // An empty stored account id records no identity to protect —
            // static tokens (ADR 0115) seal without one, and an OAuth
            // connect deliberately replaces them.
            if current.revoked_at.is_none()
                && !current.metadata.account_id.is_empty()
                && current.metadata.account_id != bundle.metadata.account_id
            {
                return Err(OAuthServiceError::AccountChanged);
            }
        }
        self.publish_bundle_at(key, bundle, current.map(|row| row.version))
            .await
    }

    /// ADR 0115: seal a user-entered personal access token as a no-refresh
    /// [`ConnectorOAuthKind::StaticToken`] bundle. The write is a deliberate
    /// replacement of any prior credential for the key (PAT over OAuth and
    /// OAuth over PAT are both intended), so no account-identity check runs;
    /// the read-version CAS still fences concurrent writers.
    ///
    /// [`ConnectorOAuthKind::StaticToken`]: engram_core::types::connector_oauth::ConnectorOAuthKind::StaticToken
    pub async fn put_static_credential(
        &self,
        key: &OAuthCredentialKey,
        secret: &str,
    ) -> Result<SealedOAuthCredential, OAuthServiceError> {
        validate_key(key)?;
        if key.subject_kind != OAuthSubjectKind::UserConnector {
            return Err(OAuthServiceError::BadRequest(
                "static credentials are restricted to user_connector subjects".into(),
            ));
        }
        let secret = secret.trim();
        if secret.is_empty() {
            return Err(OAuthServiceError::BadRequest(
                "credential value must not be empty".into(),
            ));
        }
        use engram_core::types::connector_oauth::{
            ConnectorOAuthBundle, ConnectorOAuthKind, CONNECTOR_OAUTH_BUNDLE_VERSION,
        };
        let bundle = ConnectorOAuthBundle {
            v: CONNECTOR_OAUTH_BUNDLE_VERSION,
            kind: ConnectorOAuthKind::StaticToken,
            access_token: secret.to_string(),
            token_type: "bearer".into(),
            refresh_token: None,
            scope: None,
            obtained_at: self.clock.now_utc(),
            expires_at: None,
            refresh: None,
        };
        let payload = bundle
            .to_json()
            .map_err(|_| OAuthServiceError::InvalidBundle)?;
        let current = self.meta.get_oauth_credential(key).await?;
        self.publish_bundle_at(
            key,
            ValidatedOAuthBundle {
                payload,
                // No provider-verified identity exists for a pasted token;
                // an empty account id opts out of account-switch detection.
                metadata: OAuthAccountMetadata::default(),
                expires_at: None,
            },
            current.map(|row| row.version),
        )
        .await?;
        self.meta
            .get_oauth_credential(key)
            .await?
            .ok_or(OAuthServiceError::NotFound)
    }

    pub(crate) async fn publish_bundle_at(
        &self,
        key: &OAuthCredentialKey,
        bundle: ValidatedOAuthBundle,
        expected_version: Option<i64>,
    ) -> Result<(), OAuthServiceError> {
        if bundle.payload.is_empty() || bundle.payload.len() > MAX_OAUTH_BUNDLE_BYTES {
            return Err(OAuthServiceError::InvalidBundle);
        }
        let sealed = CredCipher::new(self.kek.as_ref())
            .seal(&bundle.payload)
            .await
            .map_err(|_| OAuthServiceError::Crypto)?;
        self.meta
            .put_oauth_credential(
                NewSealedOAuthCredential {
                    key: key.clone(),
                    wrapped_dek: sealed.wrapped_dek,
                    nonce: sealed.nonce.to_vec(),
                    ciphertext: sealed.ciphertext,
                    key_id: sealed.key_id,
                    metadata: bundle.metadata,
                    expires_at: bundle.expires_at,
                },
                expected_version,
            )
            .await?;
        Ok(())
    }

    pub(crate) async fn open(
        &self,
        row: &SealedOAuthCredential,
    ) -> Result<Vec<u8>, OAuthServiceError> {
        let nonce: [u8; 12] = row
            .nonce
            .as_slice()
            .try_into()
            .map_err(|_| OAuthServiceError::InvalidBundle)?;
        CredCipher::new(self.kek.as_ref())
            .open(&SealedCred {
                wrapped_dek: row.wrapped_dek.clone(),
                nonce,
                ciphertext: row.ciphertext.clone(),
                key_id: row.key_id.clone(),
            })
            .await
            .map_err(|_| OAuthServiceError::Crypto)
    }
}

fn resolve_codex_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("ENGRAM_CODEX_BIN") {
        return PathBuf::from(path);
    }
    // Resolve before the provider subprocess clears its environment. Local
    // installs commonly live outside the platform's fallback PATH; production
    // finds the pinned image binary at /usr/local/bin/codex.
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("codex"))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/codex"))
}

pub fn spawn_cleanup(manager: Arc<OAuthManager>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = manager.clock.sleep(Duration::from_secs(30)) => {
                    if let Err(error) = manager.run_cleanup_once().await {
                        tracing::warn!(error = %error, "OAuth flow cleanup failed");
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
        }
    });
}

#[derive(Debug)]
pub enum OAuthServiceError {
    BadRequest(String),
    NotFound,
    Busy,
    OwnerLost,
    Disconnected,
    Conflict,
    AccountChanged,
    InvalidBundle,
    Crypto,
    Driver(OAuthDriverError),
    Meta(MetaError),
}

impl OAuthServiceError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::NotFound => "not_found",
            Self::Busy => "too_many_flows",
            Self::OwnerLost => "owner_lost",
            Self::Disconnected => "credential_disconnected",
            Self::Conflict => "version_conflict",
            Self::AccountChanged => "account_changed",
            Self::InvalidBundle => "invalid_bundle",
            Self::Crypto => "credential_crypto_failed",
            Self::Driver(error) => error.code,
            Self::Meta(MetaError::NotFound) => "not_found",
            Self::Meta(MetaError::Conflict(_)) => "version_conflict",
            Self::Meta(_) => "metadata_failed",
        }
    }
}

impl From<MetaError> for OAuthServiceError {
    fn from(value: MetaError) -> Self {
        Self::Meta(value)
    }
}

pub(crate) fn validate_key(key: &OAuthCredentialKey) -> Result<(), OAuthServiceError> {
    // Provider charset matches the connector-slug rule (`new_relic` carries
    // an underscore).
    if key.subject_id.trim().is_empty()
        || key.provider.is_empty()
        || !key
            .provider
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(OAuthServiceError::BadRequest(
            "invalid OAuth subject or provider".into(),
        ));
    }
    Ok(())
}

struct OpenAiCodexDriver {
    codex_bin: PathBuf,
}

struct OpenAiFlow {
    app: CodexAppServer,
    login_id: String,
    verification_url: String,
    user_code: String,
}

#[async_trait]
impl OAuthFlowHandle for OpenAiFlow {
    fn verification_url(&self) -> &str {
        &self.verification_url
    }

    fn user_code(&self) -> &str {
        &self.user_code
    }

    async fn complete(&mut self) -> Result<ValidatedOAuthBundle, OAuthDriverError> {
        self.app.wait_login(&self.login_id).await?;
        let account = self.app.read_account(true).await?;
        self.app.read_validated_bundle(account).await
    }
}

#[async_trait]
impl OAuthDriver for OpenAiCodexDriver {
    fn provider(&self) -> &'static str {
        OPENAI_CODEX_PROVIDER
    }

    async fn begin(&self) -> Result<Box<dyn OAuthFlowHandle>, OAuthDriverError> {
        let mut app = CodexAppServer::spawn(&self.codex_bin, None).await?;
        let response = app
            .request("account/login/start", json!({"type":"chatgptDeviceCode"}))
            .await?;
        let result = response.get("result").ok_or_else(|| {
            OAuthDriverError::new("malformed_provider_response", "missing result")
        })?;
        if result.get("type").and_then(Value::as_str) != Some("chatgptDeviceCode") {
            return Err(OAuthDriverError::new(
                "unsupported_auth_mode",
                "Codex did not start managed device authorization",
            ));
        }
        let required = |name: &str| {
            result
                .get(name)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .ok_or_else(|| {
                    OAuthDriverError::new("malformed_provider_response", format!("missing {name}"))
                })
        };
        Ok(Box::new(OpenAiFlow {
            login_id: required("loginId")?,
            verification_url: required("verificationUrl")?,
            user_code: required("userCode")?,
            app,
        }))
    }

    async fn validate_cache(
        &self,
        payload: &[u8],
        expected_account_id: &str,
    ) -> Result<ValidatedOAuthBundle, OAuthDriverError> {
        let parsed = parse_codex_cache(payload)?;
        if parsed.account_id != expected_account_id {
            return Err(OAuthDriverError::new(
                "account_changed",
                "refreshed credential belongs to another OpenAI account",
            ));
        }
        let mut app = CodexAppServer::spawn(&self.codex_bin, Some(payload)).await?;
        let account = app.read_account(true).await?;
        let validated = app.read_validated_bundle(account).await?;
        if validated.metadata.account_id != expected_account_id {
            return Err(OAuthDriverError::new(
                "account_changed",
                "validated credential belongs to another OpenAI account",
            ));
        }
        Ok(validated)
    }
}

struct CodexAppServer {
    _home: tempfile::TempDir,
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    buffered: Vec<Value>,
    next_id: i64,
}

fn login_result(value: &Value) -> Result<(), OAuthDriverError> {
    if value.pointer("/params/success").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(OAuthDriverError::new(
            "authorization_denied",
            "OpenAI device authorization did not complete",
        ))
    }
}

impl CodexAppServer {
    async fn spawn(bin: &Path, cache: Option<&[u8]>) -> Result<Self, OAuthDriverError> {
        let home = tempfile::Builder::new()
            .prefix("engram-oauth-")
            .tempdir()
            .map_err(|_| OAuthDriverError::new("temporary_storage_failed", "create temp dir"))?;
        if let Some(cache) = cache {
            if cache.is_empty() || cache.len() > MAX_OAUTH_BUNDLE_BYTES {
                return Err(OAuthDriverError::new(
                    "invalid_bundle",
                    "cache size rejected",
                ));
            }
            write_private(home.path().join("auth.json"), cache).await?;
        }
        let mut command = Command::new(bin);
        command
            .args(["app-server", "--stdio"])
            .env_clear()
            .env("CODEX_HOME", home.path())
            .env("CODEX_NON_INTERACTIVE", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Raw provider stderr is never logged or retained.
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|_| {
            OAuthDriverError::new("provider_unavailable", "could not start Codex app-server")
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            OAuthDriverError::new("provider_unavailable", "Codex stdin unavailable")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            OAuthDriverError::new("provider_unavailable", "Codex stdout unavailable")
        })?;
        let mut app = Self {
            _home: home,
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            buffered: Vec::new(),
            next_id: 1,
        };
        app.request(
            "initialize",
            json!({"clientInfo":{"name":"engrams-oauth","title":"Engrams","version":env!("CARGO_PKG_VERSION")}}),
        )
        .await?;
        app.notify("initialized", json!({})).await?;
        Ok(app)
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, OAuthDriverError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"id":id,"method":method,"params":params}))
            .await?;
        loop {
            let value = self.read().await?;
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                if value.get("error").is_some() {
                    return Err(OAuthDriverError::new(
                        "provider_rejected",
                        format!("Codex rejected {method}"),
                    ));
                }
                return Ok(value);
            }
            self.buffered.push(value);
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), OAuthDriverError> {
        self.write(json!({"method":method,"params":params})).await
    }

    async fn wait_login(&mut self, login_id: &str) -> Result<(), OAuthDriverError> {
        if let Some(index) = self.buffered.iter().position(|value| {
            value.get("method").and_then(Value::as_str) == Some("account/login/completed")
                && value.pointer("/params/loginId").and_then(Value::as_str) == Some(login_id)
        }) {
            let value = self.buffered.swap_remove(index);
            return login_result(&value);
        }
        loop {
            let value = self.read().await?;
            if value.get("method").and_then(Value::as_str) != Some("account/login/completed") {
                self.buffered.push(value);
                continue;
            }
            let params = value.get("params").unwrap_or(&Value::Null);
            if params.get("loginId").and_then(Value::as_str) != Some(login_id) {
                continue;
            }
            return login_result(&value);
        }
    }

    async fn read_account(&mut self, refresh: bool) -> Result<Value, OAuthDriverError> {
        let response = self
            .request("account/read", json!({"refreshToken":refresh}))
            .await?;
        let account = response
            .pointer("/result/account")
            .filter(|value| !value.is_null())
            .ok_or_else(|| OAuthDriverError::new("not_authenticated", "account is absent"))?;
        if account.get("type").and_then(Value::as_str) != Some("chatgpt") {
            return Err(OAuthDriverError::new(
                "unsupported_auth_mode",
                "managed ChatGPT authentication is required",
            ));
        }
        Ok(account.clone())
    }

    async fn read_validated_bundle(
        &self,
        account: Value,
    ) -> Result<ValidatedOAuthBundle, OAuthDriverError> {
        let payload = read_bounded_private(&self._home.path().join("auth.json")).await?;
        let parsed = parse_codex_cache(&payload)?;
        Ok(ValidatedOAuthBundle {
            payload,
            metadata: OAuthAccountMetadata {
                account_id: parsed.account_id,
                display_name: account
                    .get("email")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                plan_type: account
                    .get("planType")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                workspace_id: None,
                workspace_name: None,
            },
            // Recorded so `status()` can report `expired` instead of a
            // permanent `connected`. It does NOT pull the row into the
            // refresh sweep: that selects `connector`/`user_connector`
            // kinds only, and a harness bundle refreshes in-guest over the
            // session control channel (ADR 0115).
            expires_at: parsed.expires_at,
        })
    }

    async fn write(&mut self, value: Value) -> Result<(), OAuthDriverError> {
        let mut bytes = serde_json::to_vec(&value)
            .map_err(|_| OAuthDriverError::new("provider_protocol_failed", "encode request"))?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await.map_err(|_| {
            OAuthDriverError::new("provider_process_lost", "Codex app-server exited")
        })?;
        self.stdin
            .flush()
            .await
            .map_err(|_| OAuthDriverError::new("provider_process_lost", "Codex app-server exited"))
    }

    async fn read(&mut self) -> Result<Value, OAuthDriverError> {
        let line = self
            .lines
            .next_line()
            .await
            .map_err(|_| OAuthDriverError::new("provider_protocol_failed", "read failed"))?
            .ok_or_else(|| {
                OAuthDriverError::new("provider_process_lost", "Codex app-server exited")
            })?;
        if line.len() > MAX_OAUTH_BUNDLE_BYTES {
            return Err(OAuthDriverError::new(
                "provider_protocol_failed",
                "provider message exceeded limit",
            ));
        }
        serde_json::from_str(&line)
            .map_err(|_| OAuthDriverError::new("provider_protocol_failed", "invalid JSON"))
    }
}

impl Drop for CodexAppServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

struct ParsedCodexCache {
    account_id: String,
    /// Access-token expiry, read from the token's own `exp` claim. `None`
    /// when the claim is unreadable — the credential still works, it just
    /// stays out of expiry-derived status until the next refresh rewrites it.
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Read the `exp` claim out of a JWT access token.
///
/// The Codex auth cache records no expiry of its own (only `last_refresh`),
/// so the token itself is the only source. We decode the payload WITHOUT
/// verifying the signature: this value never authorizes anything, it only
/// feeds refresh scheduling and `status()` derivation. Any malformed input
/// yields `None` rather than an error — a provider-side format change must
/// not break login.
fn codex_access_token_expiry(access_token: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use base64::Engine as _;
    let payload = access_token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    let exp = claims.get("exp").and_then(Value::as_i64)?;
    chrono::DateTime::from_timestamp(exp, 0)
}

fn parse_codex_cache(payload: &[u8]) -> Result<ParsedCodexCache, OAuthDriverError> {
    if payload.is_empty() || payload.len() > MAX_OAUTH_BUNDLE_BYTES {
        return Err(OAuthDriverError::new(
            "invalid_bundle",
            "cache size rejected",
        ));
    }
    let value: Value = serde_json::from_slice(payload)
        .map_err(|_| OAuthDriverError::new("invalid_bundle", "cache is not JSON"))?;
    if value.get("auth_mode").and_then(Value::as_str) != Some("chatgpt") {
        return Err(OAuthDriverError::new(
            "unsupported_auth_mode",
            "managed ChatGPT authentication is required",
        ));
    }
    if value
        .get("OPENAI_API_KEY")
        .is_some_and(|value| !value.is_null())
    {
        return Err(OAuthDriverError::new(
            "unsupported_auth_mode",
            "API-key cache is not an OAuth connection",
        ));
    }
    let tokens = value
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| OAuthDriverError::new("invalid_bundle", "token cache missing"))?;
    for required in ["access_token", "refresh_token"] {
        if tokens
            .get(required)
            .and_then(Value::as_str)
            .is_none_or(|value| value.is_empty())
        {
            return Err(OAuthDriverError::new(
                "invalid_bundle",
                "token cache incomplete",
            ));
        }
    }
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| OAuthDriverError::new("invalid_bundle", "account identity missing"))?
        .to_string();
    let expires_at = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .and_then(codex_access_token_expiry);
    Ok(ParsedCodexCache {
        account_id,
        expires_at,
    })
}

async fn write_private(path: PathBuf, payload: &[u8]) -> Result<(), OAuthDriverError> {
    use tokio::io::AsyncWriteExt as _;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .await
        .map_err(|_| OAuthDriverError::new("temporary_storage_failed", "create cache"))?;
    file.write_all(payload)
        .await
        .map_err(|_| OAuthDriverError::new("temporary_storage_failed", "write cache"))?;
    file.flush()
        .await
        .map_err(|_| OAuthDriverError::new("temporary_storage_failed", "flush cache"))
}

async fn read_bounded_private(path: &Path) -> Result<Vec<u8>, OAuthDriverError> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| OAuthDriverError::new("invalid_bundle", "credential cache missing"))?;
    if !metadata.file_type().is_file() || metadata.len() as usize > MAX_OAUTH_BUNDLE_BYTES {
        return Err(OAuthDriverError::new(
            "invalid_bundle",
            "credential cache type or size rejected",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(OAuthDriverError::new(
                "invalid_bundle",
                "credential cache permissions are too broad",
            ));
        }
    }
    let payload = tokio::fs::read(path)
        .await
        .map_err(|_| OAuthDriverError::new("invalid_bundle", "credential cache unreadable"))?;
    if payload.len() > MAX_OAUTH_BUNDLE_BYTES {
        return Err(OAuthDriverError::new(
            "invalid_bundle",
            "credential cache too large",
        ));
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn fake_codex_source(source: &str) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("codex");
        std::fs::write(&script, source).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[cfg(unix)]
    fn fake_codex(plan: &str, success: bool) -> tempfile::TempDir {
        let source = format!(
            r#"#!/bin/sh
umask 077
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{{"id":1,"result":{{}}}}' ;;
    *'"method":"account/login/start"'*)
      printf '%s' '{{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{{"access_token":"access-secret","refresh_token":"refresh-secret","account_id":"acct-1"}}}}' > "$CODEX_HOME/auth.json"
      printf '%s\n' '{{"id":2,"result":{{"type":"chatgptDeviceCode","loginId":"login-1","verificationUrl":"https://auth.openai.test/device","userCode":"ABCD-EFGH"}}}}'
      printf '%s\n' '{{"method":"account/login/completed","params":{{"loginId":"login-1","success":{success}}}}}' ;;
    *'"method":"account/read"'*) printf '%s\n' '{{"id":3,"result":{{"account":{{"type":"chatgpt","email":"person@example.com","planType":"{plan}"}}}}}}' ;;
  esac
done
"#,
        );
        fake_codex_source(&source)
    }

    #[test]
    fn cache_validation_requires_managed_chatgpt_and_account_identity() {
        let good = serde_json::to_vec(&json!({
            "auth_mode":"chatgpt",
            "OPENAI_API_KEY":null,
            "tokens":{
                "access_token":"access-secret",
                "refresh_token":"refresh-secret",
                "account_id":"acct-personal"
            }
        }))
        .unwrap();
        assert_eq!(
            parse_codex_cache(&good).unwrap().account_id,
            "acct-personal"
        );
        // A non-JWT access token is still a usable credential; it simply
        // carries no expiry, so status derivation stays silent about it.
        assert!(parse_codex_cache(&good).unwrap().expires_at.is_none());

        for bad in [
            json!({"auth_mode":"apikey","tokens":{}}),
            json!({"auth_mode":"chatgpt","OPENAI_API_KEY":"secret","tokens":{}}),
            json!({"auth_mode":"chatgpt","tokens":{"access_token":"x","refresh_token":"y"}}),
        ] {
            assert!(parse_codex_cache(&serde_json::to_vec(&bad).unwrap()).is_err());
        }
    }

    #[test]
    fn bundle_limit_rejects_oversized_input() {
        assert!(parse_codex_cache(&vec![b'x'; MAX_OAUTH_BUNDLE_BYTES + 1]).is_err());
    }

    /// Build an unsigned JWT carrying `exp`, in the shape a real ChatGPT
    /// access token uses: base64url, no padding, three dot-separated parts.
    fn jwt_with_exp(exp: i64) -> String {
        use base64::Engine as _;
        let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        format!(
            "{}.{}.{}",
            b64(br#"{"alg":"none"}"#),
            b64(json!({ "exp": exp, "iat": exp - 864_000 })
                .to_string()
                .as_bytes()),
            "not-verified"
        )
    }

    #[test]
    fn access_token_expiry_comes_from_the_exp_claim() {
        // Verified against a live Codex credential: the access token is a
        // JWT and its lifetime is 10 days. The cache itself records no
        // expiry, so this claim is the only source.
        let exp = 1_787_334_053;
        assert_eq!(
            codex_access_token_expiry(&jwt_with_exp(exp)),
            chrono::DateTime::from_timestamp(exp, 0)
        );

        let cache = serde_json::to_vec(&json!({
            "auth_mode":"chatgpt",
            "OPENAI_API_KEY":null,
            "tokens":{
                "access_token": jwt_with_exp(exp),
                "refresh_token":"refresh-secret",
                "account_id":"acct-personal"
            }
        }))
        .unwrap();
        assert_eq!(
            parse_codex_cache(&cache).unwrap().expires_at,
            chrono::DateTime::from_timestamp(exp, 0)
        );
    }

    #[test]
    fn unreadable_access_token_expiry_degrades_to_none() {
        // Never an error: the expiry only feeds status derivation, so a
        // provider-side format change must not break the login path.
        for token in [
            "",
            "opaque-not-a-jwt",
            "only.two",
            "a.!!!not-base64!!!.c",
            // Valid base64url payload, but no `exp` claim.
            &format!("a.{}.c", {
                use base64::Engine as _;
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"iat":1}"#)
            }),
        ] {
            assert!(
                codex_access_token_expiry(token).is_none(),
                "expected no expiry for {token:?}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_app_server_accepts_personal_and_enterprise_managed_accounts() {
        for plan in ["plus", "enterprise"] {
            let fake = fake_codex(plan, true);
            let driver = OpenAiCodexDriver {
                codex_bin: fake.path().join("codex"),
            };
            let mut flow = driver.begin().await.unwrap();
            assert_eq!(flow.verification_url(), "https://auth.openai.test/device");
            assert_eq!(flow.user_code(), "ABCD-EFGH");
            let bundle = flow.complete().await.unwrap();
            assert_eq!(bundle.metadata.account_id, "acct-1");
            assert_eq!(bundle.metadata.plan_type.as_deref(), Some(plan));
            assert!(!String::from_utf8_lossy(&bundle.payload).contains("ABCD-EFGH"));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_app_server_surfaces_device_authorization_denial() {
        let fake = fake_codex("plus", false);
        let driver = OpenAiCodexDriver {
            codex_bin: fake.path().join("codex"),
        };
        let mut flow = driver.begin().await.unwrap();
        let error = match flow.complete().await {
            Err(error) => error,
            Ok(_) => panic!("denied flow unexpectedly succeeded"),
        };
        assert_eq!(error.code, "authorization_denied");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_app_server_surfaces_disabled_device_flow_without_raw_error() {
        let fake = fake_codex_source(
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"method":"account/login/start"'*)
      printf '%s\n' '{"id":2,"error":{"message":"device login disabled token=raw-secret"}}'
      exit 0 ;;
  esac
done
"#,
        );
        let driver = OpenAiCodexDriver {
            codex_bin: fake.path().join("codex"),
        };
        let error = match driver.begin().await {
            Err(error) => error,
            Ok(_) => panic!("disabled device flow unexpectedly started"),
        };
        assert_eq!(error.code, "provider_rejected");
        assert!(!error.detail.contains("raw-secret"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_app_server_surfaces_subprocess_loss() {
        let fake = fake_codex_source(
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"method":"account/login/start"'*)
      printf '%s\n' '{"id":2,"result":{"type":"chatgptDeviceCode","loginId":"login-1","verificationUrl":"https://auth.openai.test/device","userCode":"ABCD-EFGH"}}'
      exit 0 ;;
  esac
done
"#,
        );
        let driver = OpenAiCodexDriver {
            codex_bin: fake.path().join("codex"),
        };
        let mut flow = driver.begin().await.unwrap();
        let error = match flow.complete().await {
            Err(error) => error,
            Ok(_) => panic!("lost subprocess unexpectedly completed"),
        };
        assert_eq!(error.code, "provider_process_lost");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refreshed_cache_cannot_switch_openai_accounts() {
        let fake = fake_codex("enterprise", true);
        let driver = OpenAiCodexDriver {
            codex_bin: fake.path().join("codex"),
        };
        let payload = serde_json::to_vec(&json!({
            "auth_mode":"chatgpt",
            "OPENAI_API_KEY":null,
            "tokens":{
                "access_token":"access-secret",
                "refresh_token":"refresh-secret",
                "account_id":"acct-other"
            }
        }))
        .unwrap();
        let error = match driver.validate_cache(&payload, "acct-1").await {
            Err(error) => error,
            Ok(_) => panic!("account switch unexpectedly validated"),
        };
        assert_eq!(error.code, "account_changed");
    }
}
