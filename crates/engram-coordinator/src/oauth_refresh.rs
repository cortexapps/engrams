//! ADR 0106 addendum: proactive refresh for rotating connector OAuth
//! credentials (Linear: 24 h access tokens, mandatory rotating refresh
//! tokens), plus the one resolution seam every delivery consumer calls.
//!
//! Two rails share one refresh function:
//! - the background scanner (`spawn_connector_refresh` wrapping
//!   `run_connector_refresh_once`, ADR 0098 spawn/run_once split) refreshes
//!   due rows hours ahead of expiry, so consumers rarely observe staleness;
//! - `resolve_connector_token` is the on-demand backstop: it refreshes
//!   inline (single-flighted per key) when a bundle is inside its margin,
//!   and NEVER errors while a usable-or-stale token exists.
//!
//! The CAS-loser rule is load-bearing under rotation: `Conflict` on the
//! bundle write — or `invalid_grant` after the row version moved — means a
//! concurrent refresh won; reload the winner. Only `invalid_grant` with the
//! version unchanged marks the credential broken (reconnect required).

use std::sync::Arc;
use std::time::Duration;

use engram_core::types::connector_oauth::{ConnectorOAuthBundle, ConnectorOAuthRefreshSpec};
use engram_core::types::oauth::{OAuthCredentialKey, OAuthSubjectKind, SealedOAuthCredential};

use crate::oauth::{OAuthManager, OAuthServiceError, ValidatedOAuthBundle};

/// Widest lookahead the due-list query uses. Rows inside the horizon are
/// examined; whether one actually refreshes is the per-bundle margin below.
const SWEEP_HORIZON: chrono::Duration = chrono::Duration::hours(6);
/// Advisory claim length: long enough for one refresh round-trip, short
/// enough that a crashed replica's claim lapses within a sweep or two.
const CLAIM_TTL: chrono::Duration = chrono::Duration::minutes(5);
const SWEEP_PERIOD: Duration = Duration::from_secs(60);
const SWEEP_BATCH: i64 = 32;

/// The one resolution seam. `secret` is the raw access token; consumers
/// render their own header template around it.
#[derive(Clone, Debug)]
pub struct ResolvedConnectorToken {
    pub secret: String,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub version: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RefreshSweep {
    pub examined: usize,
    pub refreshed: usize,
    pub broke: usize,
    pub lost_races: usize,
    pub skipped: usize,
    pub transient_failures: usize,
}

/// Refresh when the remaining validity is inside `max(30 min, 25% of TTL)`,
/// clamped so short-lived tokens are not refreshed in a loop.
fn within_refresh_margin(
    bundle: &ConnectorOAuthBundle,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let Some(expires_at) = bundle.expires_at else {
        return false;
    };
    let ttl = (expires_at - bundle.obtained_at).max(chrono::Duration::zero());
    let floor = chrono::Duration::minutes(30).min(ttl / 2);
    let margin = (ttl / 4).max(floor);
    expires_at - now <= margin
}

enum RefreshOutcome {
    /// A fresh row was published (by us or by a winning racer).
    Row(Box<SealedOAuthCredential>),
    /// The provider terminally rejected the grant; the row is now broken.
    Broken,
}

impl OAuthManager {
    /// Refresh one credential's bundle. `row` is the caller's snapshot; a
    /// version that moved underneath means a racer won and we return the
    /// winner. Transient provider failures bubble as `Err`.
    async fn refresh_credential(
        &self,
        row: &SealedOAuthCredential,
    ) -> Result<RefreshOutcome, OAuthServiceError> {
        let payload = self.open(row).await?;
        let bundle = ConnectorOAuthBundle::from_json(&payload)
            .map_err(|_| OAuthServiceError::InvalidBundle)?;
        let (Some(refresh_token), Some(refresh)) = (&bundle.refresh_token, &bundle.refresh) else {
            return Ok(RefreshOutcome::Row(Box::new(row.clone())));
        };
        let client_id = self.resolve_refresh_secret(&refresh.client_id_ref).await?;
        let client_secret = self
            .resolve_refresh_secret(&refresh.client_secret_ref)
            .await?;
        let form: Vec<(&str, String)> = vec![
            ("grant_type", "refresh_token".into()),
            ("refresh_token", refresh_token.clone()),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ];
        let delimiter = ",";
        match self.token_grant(&refresh.token_url, &form, delimiter).await {
            Ok(grant) => {
                let new_bundle = ConnectorOAuthBundle {
                    v: bundle.v,
                    kind: bundle.kind,
                    access_token: grant.access_token,
                    token_type: grant.token_type,
                    // Rotation: adopt the new refresh token; a provider that
                    // does not rotate keeps the old one.
                    refresh_token: grant.refresh_token.or_else(|| Some(refresh_token.clone())),
                    scope: grant.scope.or(bundle.scope),
                    obtained_at: self.clock.now_utc(),
                    expires_at: grant.expires_at,
                    refresh: Some(ConnectorOAuthRefreshSpec {
                        token_url: refresh.token_url.clone(),
                        client_id_ref: refresh.client_id_ref.clone(),
                        client_secret_ref: refresh.client_secret_ref.clone(),
                    }),
                };
                let payload = new_bundle
                    .to_json()
                    .map_err(|_| OAuthServiceError::InvalidBundle)?;
                let publish = self
                    .publish_bundle_at(
                        &row.key,
                        ValidatedOAuthBundle {
                            payload,
                            // Refresh does not change account identity.
                            metadata: row.metadata.clone(),
                            expires_at: new_bundle.expires_at,
                        },
                        Some(row.version),
                    )
                    .await;
                match publish {
                    Ok(()) => {}
                    Err(OAuthServiceError::Meta(engram_core::MetaError::Conflict(_)))
                    | Err(OAuthServiceError::Conflict) => {
                        // CAS loser: a concurrent refresh rotated first. The
                        // provider's 30-min rotation replay grace makes our
                        // duplicate exchange harmless; adopt the winner.
                    }
                    Err(error) => return Err(error),
                }
                let winner = self
                    .meta
                    .get_oauth_credential(&row.key)
                    .await?
                    .ok_or(OAuthServiceError::NotFound)?;
                Ok(RefreshOutcome::Row(Box::new(winner)))
            }
            Err(OAuthServiceError::Driver(driver)) if driver.code == "invalid_grant" => {
                match self
                    .meta
                    .mark_oauth_credential_broken(&row.key, row.version, "invalid_grant")
                    .await
                {
                    Ok(_) => Ok(RefreshOutcome::Broken),
                    Err(engram_core::MetaError::Conflict(_)) => {
                        // The version moved: a racer refreshed successfully
                        // and OUR grant was the stale rotated-out token.
                        // The credential is healthy — reload the winner.
                        let winner = self
                            .meta
                            .get_oauth_credential(&row.key)
                            .await?
                            .ok_or(OAuthServiceError::NotFound)?;
                        Ok(RefreshOutcome::Row(Box::new(winner)))
                    }
                    Err(error) => Err(error.into()),
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn resolve_refresh_secret(&self, name: &str) -> Result<String, OAuthServiceError> {
        use engram_core::traits::SecretContext;
        use engram_core::types::SecretSchema;
        let ctx = SecretContext {
            repo: "",
            image_tag: "",
        };
        let schema = SecretSchema {
            required: true,
            ..Default::default()
        };
        self.secrets
            .get(&ctx, name, &schema)
            .await
            .map_err(|e| {
                OAuthServiceError::BadRequest(format!("could not resolve org secret: {e}"))
            })?
            .ok_or_else(|| OAuthServiceError::BadRequest(format!("org secret {name:?} is not set")))
    }

    /// One scanner sweep over due connector credentials. Pure step — the
    /// timer loop is a thin wrapper; tests and the simulator drive this
    /// directly (ADR 0098).
    pub async fn run_connector_refresh_once(&self) -> Result<RefreshSweep, OAuthServiceError> {
        let now = self.clock.now_utc();
        let due = self
            .meta
            .list_oauth_credentials_due_for_refresh(
                OAuthSubjectKind::Connector,
                now,
                now + SWEEP_HORIZON,
                SWEEP_BATCH,
            )
            .await?;
        let mut sweep = RefreshSweep {
            examined: due.len(),
            ..RefreshSweep::default()
        };
        for row in due {
            if !self
                .meta
                .claim_oauth_refresh(&row.key, now, now + CLAIM_TTL)
                .await?
            {
                sweep.lost_races += 1;
                continue;
            }
            // The horizon is wider than any single bundle's margin; check
            // the real margin after unsealing. Any per-row defect skips THIS
            // row only — one bad credential must not abort the batch behind
            // it (the claim rate-limits re-examination to once per CLAIM_TTL).
            let payload = match self.open(&row).await {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!(
                        provider = %row.key.provider,
                        code = error.code(),
                        "connector OAuth bundle failed to unseal; skipping refresh"
                    );
                    sweep.skipped += 1;
                    continue;
                }
            };
            let bundle = match ConnectorOAuthBundle::from_json(&payload) {
                Ok(bundle) => bundle,
                Err(error) => {
                    tracing::warn!(
                        provider = %row.key.provider,
                        error = %error,
                        "connector OAuth bundle failed to parse; skipping refresh"
                    );
                    sweep.skipped += 1;
                    continue;
                }
            };
            if !bundle.refreshable() {
                // An expiring token WITHOUT a refresh token has no repair
                // path. While it is still valid, leave it alone (the claim
                // bounds re-checks until expiry). Once it is past expiry,
                // mark it broken: it leaves the due set, resolution stops
                // serving a dead token, and the admin sees needs-reconnect
                // instead of a "connected" lie.
                if bundle.expires_at.is_some_and(|at| at <= now) {
                    match self
                        .meta
                        .mark_oauth_credential_broken(&row.key, row.version, "no_refresh_token")
                        .await
                    {
                        Ok(_) => {
                            tracing::warn!(
                                provider = %row.key.provider,
                                "connector OAuth token expired with no refresh token; reconnect required"
                            );
                            sweep.broke += 1;
                        }
                        // The version moved: a reconnect or refresh won.
                        Err(engram_core::MetaError::Conflict(_)) => sweep.lost_races += 1,
                        Err(error) => {
                            tracing::warn!(
                                provider = %row.key.provider,
                                error = %error,
                                "could not mark a non-refreshable expired credential broken"
                            );
                            sweep.transient_failures += 1;
                        }
                    }
                } else {
                    sweep.skipped += 1;
                }
                continue;
            }
            if !within_refresh_margin(&bundle, now) {
                sweep.skipped += 1;
                continue;
            }
            match self.refresh_credential(&row).await {
                Ok(RefreshOutcome::Row(_)) => sweep.refreshed += 1,
                Ok(RefreshOutcome::Broken) => {
                    tracing::warn!(
                        provider = %row.key.provider,
                        "connector OAuth refresh terminally rejected; reconnect required"
                    );
                    sweep.broke += 1;
                }
                Err(error) => {
                    // Transient: keep serving the stale token; the claim
                    // lapses and the next sweep retries.
                    tracing::warn!(
                        provider = %row.key.provider,
                        code = error.code(),
                        "connector OAuth refresh failed transiently"
                    );
                    sweep.transient_failures += 1;
                }
            }
        }
        Ok(sweep)
    }

    /// The delivery seam: session boot, the egress refresh route, and Mode
    /// A/B all resolve through here. Refreshes inline (single-flighted) when
    /// the bundle is inside its margin; on transient refresh failure the
    /// STALE token is returned — an expired token yields a provider 401 the
    /// caller can surface, while a dropped request cannot be recovered.
    /// Errors only when no usable credential exists at all.
    pub async fn resolve_connector_token(
        &self,
        key: &OAuthCredentialKey,
    ) -> Result<ResolvedConnectorToken, OAuthServiceError> {
        let row = self
            .meta
            .get_oauth_credential(key)
            .await?
            .ok_or(OAuthServiceError::NotFound)?;
        if row.revoked_at.is_some() {
            return Err(OAuthServiceError::Disconnected);
        }
        if row.broken_at.is_some() {
            return Err(OAuthServiceError::Disconnected);
        }
        let payload = self.open(&row).await?;
        let bundle = ConnectorOAuthBundle::from_json(&payload)
            .map_err(|_| OAuthServiceError::InvalidBundle)?;
        let now = self.clock.now_utc();
        if !(bundle.refreshable() && within_refresh_margin(&bundle, now)) {
            return Ok(ResolvedConnectorToken {
                secret: bundle.access_token,
                expires_at: bundle.expires_at,
                version: row.version,
            });
        }

        // Single-flight: one refresh per key per process; followers re-read.
        let flight = self
            .refresh_flights
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = flight.lock().await;
        let current = self
            .meta
            .get_oauth_credential(key)
            .await?
            .ok_or(OAuthServiceError::NotFound)?;
        if current.version != row.version {
            // A flight-mate (or another replica) already refreshed.
            return self.token_from_row(&current).await;
        }
        // Cross-replica politeness only; failure to claim does not block an
        // inline refresh — the CAS arbitrates.
        let _ = self
            .meta
            .claim_oauth_refresh(key, now, now + CLAIM_TTL)
            .await;
        match self.refresh_credential(&current).await {
            Ok(RefreshOutcome::Row(fresh)) => self.token_from_row(&fresh).await,
            Ok(RefreshOutcome::Broken) => Err(OAuthServiceError::Disconnected),
            Err(error) => {
                tracing::warn!(
                    provider = %key.provider,
                    code = error.code(),
                    "on-demand connector refresh failed; serving the stale token"
                );
                Ok(ResolvedConnectorToken {
                    secret: bundle.access_token,
                    expires_at: bundle.expires_at,
                    version: row.version,
                })
            }
        }
    }

    async fn token_from_row(
        &self,
        row: &SealedOAuthCredential,
    ) -> Result<ResolvedConnectorToken, OAuthServiceError> {
        if row.revoked_at.is_some() || row.broken_at.is_some() {
            return Err(OAuthServiceError::Disconnected);
        }
        let payload = self.open(row).await?;
        let bundle = ConnectorOAuthBundle::from_json(&payload)
            .map_err(|_| OAuthServiceError::InvalidBundle)?;
        Ok(ResolvedConnectorToken {
            secret: bundle.access_token,
            expires_at: bundle.expires_at,
            version: row.version,
        })
    }
}

/// Timer wrapper around `run_connector_refresh_once` (ADR 0098: the loop is
/// thin; the step is the unit tests and the simulator drive).
pub fn spawn_connector_refresh(
    manager: Arc<OAuthManager>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = manager.clock.sleep(SWEEP_PERIOD) => {
                    match manager.run_connector_refresh_once().await {
                        Ok(sweep) if sweep.examined > 0 => {
                            tracing::debug!(?sweep, "connector OAuth refresh sweep");
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(code = error.code(), "connector OAuth refresh sweep failed");
                        }
                    }
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::connector_oauth::ConnectorOAuthKind;

    fn bundle(obtained: &str, expires: Option<&str>) -> ConnectorOAuthBundle {
        ConnectorOAuthBundle {
            v: 1,
            kind: ConnectorOAuthKind::Oauth2AuthorizationCode,
            access_token: "at".into(),
            token_type: "bearer".into(),
            refresh_token: Some("rt".into()),
            scope: None,
            obtained_at: chrono::DateTime::parse_from_rfc3339(obtained)
                .unwrap()
                .with_timezone(&chrono::Utc),
            expires_at: expires.map(|e| {
                chrono::DateTime::parse_from_rfc3339(e)
                    .unwrap()
                    .with_timezone(&chrono::Utc)
            }),
            refresh: Some(ConnectorOAuthRefreshSpec {
                token_url: "https://x/token".into(),
                client_id_ref: "a".into(),
                client_secret_ref: "b".into(),
            }),
        }
    }

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn margin_is_quarter_ttl_for_day_tokens() {
        let b = bundle("2026-08-03T00:00:00Z", Some("2026-08-04T00:00:00Z"));
        // 24h TTL -> 6h margin: not due at 17:59, due at 18:01.
        assert!(!within_refresh_margin(&b, at("2026-08-03T17:59:00Z")));
        assert!(within_refresh_margin(&b, at("2026-08-03T18:01:00Z")));
        // An already-expired bundle is always within margin.
        assert!(within_refresh_margin(&b, at("2026-08-05T00:00:00Z")));
    }

    #[test]
    fn margin_never_swallows_short_tokens() {
        // 20-minute TTL: floor clamps to ttl/2 = 10 min, margin = max(5,10).
        let b = bundle("2026-08-03T00:00:00Z", Some("2026-08-03T00:20:00Z"));
        assert!(!within_refresh_margin(&b, at("2026-08-03T00:05:00Z")));
        assert!(within_refresh_margin(&b, at("2026-08-03T00:11:00Z")));
    }

    #[test]
    fn non_expiring_bundles_never_refresh() {
        let b = bundle("2026-08-03T00:00:00Z", None);
        assert!(!within_refresh_margin(&b, at("2030-01-01T00:00:00Z")));
    }
}
