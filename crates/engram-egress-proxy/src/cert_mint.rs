//! Per-SNI leaf cert mint, signed by the host CA, cached for the
//! proxy's lifetime.
//!
//! When we MITM a TLS connection, rustls needs a server cert that
//! authenticates the SNI we're impersonating. We mint one on demand,
//! signed by the CA the guest already trusts (installed via the
//! harness substrate's `/.engram-host/ca.pem`). After mint we cache
//! the resulting `CertifiedKey` so subsequent connections to the
//! same host reuse the same leaf.
//!
//! Each leaf is valid for [`LEAF_VALIDITY`]. Short enough that a key
//! compromise has limited blast radius; long enough that we don't
//! re-mint on every connection. The cache honours that window — see
//! [`CachedLeaf`] for why a cache that didn't was a latent outage.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use rustls::crypto::ring::sign::any_supported_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use time::OffsetDateTime;

use crate::ca::Ca;

/// How long a freshly minted leaf is valid for. Slightly over a day so
/// we don't churn at exactly the same wall-clock time tomorrow.
const LEAF_VALIDITY: Duration = Duration::hours(25);

/// How far `not_before` is backdated. Covers ordinary clock skew between
/// this host and the guest validating the leaf.
///
/// It is deliberately small. A guest's clock is not left to chance —
/// the host steps it at `start_agent` (see `engram_agentd::clock` and
/// `FirecrackerBackend::step_guest_clock`) — so this is a margin for
/// jitter, not a substitute for that. Widening it to paper over a guest
/// whose clock is genuinely wrong would only delay the same failure and
/// hide the signal.
const CLOCK_SKEW_GRACE: Duration = Duration::minutes(5);

/// Re-mint a cached leaf once it has less than this left to live, so a
/// long-lived connection can't be handed a cert that expires mid-use.
const REMINT_MARGIN: Duration = Duration::hours(1);

/// A minted leaf plus the expiry it was minted with.
///
/// The expiry is the point. An earlier version cached the
/// `CertifiedKey` alone, forever, and paired that with a `not_after`
/// rounded DOWN to midnight — so a leaf minted at 23:00 was valid for
/// one hour and then served, expired, for the life of the process. The
/// two bugs hid each other: the truncation made the window wrong, and
/// the immortal cache made sure nothing ever re-derived it.
struct CachedLeaf {
    key: Arc<CertifiedKey>,
    not_after: DateTime<Utc>,
}

pub struct CertMint {
    ca: Arc<Ca>,
    cache: Mutex<HashMap<String, CachedLeaf>>,
}

impl CertMint {
    pub fn new(ca: Arc<Ca>) -> Self {
        Self {
            ca,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Get-or-mint the leaf for `hostname`. Cached: subsequent calls
    /// for the same host return the same `CertifiedKey`.
    pub fn leaf_for(&self, hostname: &str) -> Result<Arc<CertifiedKey>, MintError> {
        self.leaf_for_at(hostname, crate::time_source::wall_now())
    }

    /// [`leaf_for`](Self::leaf_for) with `now` supplied, so the cache's
    /// expiry policy is testable without waiting a day for it.
    fn leaf_for_at(
        &self,
        hostname: &str,
        now: DateTime<Utc>,
    ) -> Result<Arc<CertifiedKey>, MintError> {
        let key = hostname.to_ascii_lowercase();
        if let Some(c) = self.cache.lock().get(&key) {
            if c.not_after - now > REMINT_MARGIN {
                return Ok(c.key.clone());
            }
        }
        let (minted, not_after) = self.mint(&key, now)?;
        self.cache.lock().insert(
            key,
            CachedLeaf {
                key: minted.clone(),
                not_after,
            },
        );
        Ok(minted)
    }

    /// Mint a leaf for `hostname` valid over [`validity_window`] from
    /// `now`. Returns the leaf and its `not_after`, so the caller caches
    /// the window it actually minted rather than re-deriving it.
    fn mint(
        &self,
        hostname: &str,
        now: DateTime<Utc>,
    ) -> Result<(Arc<CertifiedKey>, DateTime<Utc>), MintError> {
        let mut params = CertificateParams::new(vec![hostname.to_string()])
            .map_err(|e| MintError::Rcgen(format!("leaf params: {e}")))?;
        params.distinguished_name = {
            let mut dn = DistinguishedName::new();
            dn.push(DnType::CommonName, hostname);
            dn
        };
        params.subject_alt_names =
            vec![SanType::DnsName(hostname.to_string().try_into().map_err(
                |_| MintError::Rcgen(format!("invalid hostname `{hostname}`")),
            )?)];
        let (not_before, not_after) = validity_window(now);
        params.not_before = to_offset_date_time(not_before)?;
        params.not_after = to_offset_date_time(not_after)?;

        let leaf_kp =
            KeyPair::generate().map_err(|e| MintError::Rcgen(format!("leaf keygen: {e}")))?;
        // Derive the issuer from the CA's persisted PEM — the same bytes the
        // guest trusts — so the leaf's issuer DN always matches the delivered
        // cert's subject DN. Cheap: one PEM parse, no signing.
        let issuer = self
            .ca
            .issuer()
            .map_err(|e| MintError::Rcgen(format!("ca issuer: {e}")))?;
        let leaf = params
            .signed_by(&leaf_kp, &issuer)
            .map_err(|e| MintError::Rcgen(format!("leaf sign: {e}")))?;

        let leaf_der = leaf.der().to_vec();
        let key_der = leaf_kp.serialize_der();
        let cert = CertificateDer::from(leaf_der);
        let key: PrivateKeyDer<'static> = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
        let signing_key = any_supported_type(&key)
            .map_err(|e| MintError::Rustls(format!("any_supported_type: {e}")))?;
        Ok((
            Arc::new(CertifiedKey::new(vec![cert], signing_key)),
            not_after,
        ))
    }

    pub fn cache_size(&self) -> usize {
        self.cache.lock().len()
    }
}

/// The `(not_before, not_after)` a leaf minted at `now` carries.
///
/// Pure, and separated from [`CertMint::mint`] precisely because the bug
/// it replaces lived in arithmetic nobody could see. The old code fed
/// both ends through `rcgen::date_time_ymd`, which keeps only the
/// year/month/day and drops the time to midnight UTC. That silently
/// turned a nominal 25-hour window into one that ran from midnight
/// TODAY to midnight of the day `now + 25h` lands on — so the real
/// lifetime swung between ~1 hour (minted at 23:00) and ~48 hours
/// (minted at 00:30) depending only on what time of day the process
/// happened to mint, and `CLOCK_SKEW_GRACE` was rounded away entirely.
fn validity_window(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    (now - CLOCK_SKEW_GRACE, now + LEAF_VALIDITY)
}

/// chrono → `time`, the calendar type rcgen's params take. Second
/// resolution: certificate validity is expressed in whole seconds on the
/// wire anyway.
fn to_offset_date_time(t: DateTime<Utc>) -> Result<OffsetDateTime, MintError> {
    OffsetDateTime::from_unix_timestamp(t.timestamp())
        .map_err(|e| MintError::Rcgen(format!("validity bound {t} out of range: {e}")))
}

#[derive(Debug)]
pub enum MintError {
    Rcgen(String),
    Rustls(String),
}

impl std::fmt::Display for MintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rcgen(s) => write!(f, "rcgen: {s}"),
            Self::Rustls(s) => write!(f, "rustls: {s}"),
        }
    }
}

impl std::error::Error for MintError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ca::Ca;
    use chrono::TimeZone;

    fn ca() -> Arc<Ca> {
        let tmp = tempfile::tempdir().unwrap().keep();
        Arc::new(Ca::load_or_generate(&tmp).unwrap())
    }

    #[test]
    fn mints_then_caches() {
        let m = CertMint::new(ca());
        let a = m.leaf_for("api.github.com").unwrap();
        let b = m.leaf_for("API.GitHub.COM").unwrap();
        // Lowercase normalization → same cache entry.
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(m.cache_size(), 1);
        let c = m.leaf_for("api.openai.com").unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(m.cache_size(), 2);
    }

    /// The minted certificate must carry the window we asked for, to the
    /// second — read back off the DER, not off `validity_window`, because
    /// the bug this replaces was entirely in the hand-off to rcgen. A
    /// mint at 23:00 is the specific case the old midnight-truncating
    /// code got worst: it produced a cert that expired in one hour.
    #[test]
    fn minted_der_carries_the_exact_window() {
        use x509_parser::prelude::FromDer;

        let m = CertMint::new(ca());
        let now = Utc.with_ymd_and_hms(2026, 8, 7, 23, 0, 0).unwrap();
        let leaf = m.leaf_for_at("api.anthropic.com", now).unwrap();

        let (_, parsed) =
            x509_parser::certificate::X509Certificate::from_der(leaf.cert[0].as_ref()).unwrap();
        let not_before = parsed.validity().not_before.timestamp();
        let not_after = parsed.validity().not_after.timestamp();

        assert_eq!(not_before, (now - CLOCK_SKEW_GRACE).timestamp());
        assert_eq!(not_after, (now + LEAF_VALIDITY).timestamp());
        // The regression, stated as the property that matters: the cert is
        // good for essentially the whole nominal validity, whatever time of
        // day it was minted. The old code yielded 3600s here.
        assert_eq!(
            not_after - not_before,
            (LEAF_VALIDITY + CLOCK_SKEW_GRACE).num_seconds()
        );
    }

    /// `not_before` must already be in the past when the cert is minted —
    /// a leaf a guest cannot use until later is the exact shape of the
    /// `SSL certificate is not yet valid` failure this file has to avoid.
    #[test]
    fn not_before_is_never_in_the_future() {
        for hour in 0..24 {
            let now = Utc.with_ymd_and_hms(2026, 8, 7, hour, 30, 0).unwrap();
            let (not_before, not_after) = validity_window(now);
            assert!(
                not_before < now,
                "hour {hour}: not_before must be backdated"
            );
            assert!(
                not_after > now,
                "hour {hour}: not_after must be in the future"
            );
        }
    }

    /// A cached leaf is reused while it has life left, and re-minted once
    /// it nears expiry — the cache must never outlive what it holds.
    #[test]
    fn cache_reuses_until_expiry_then_remints() {
        let m = CertMint::new(ca());
        let t0 = Utc.with_ymd_and_hms(2026, 8, 7, 9, 0, 0).unwrap();
        let a = m.leaf_for_at("api.anthropic.com", t0).unwrap();

        // Comfortably inside the window: same leaf, no re-mint.
        let b = m
            .leaf_for_at("api.anthropic.com", t0 + Duration::hours(12))
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(m.cache_size(), 1);

        // Inside REMINT_MARGIN of expiry: re-minted before it can be
        // served expired, and the cache entry is replaced, not added to.
        let c = m
            .leaf_for_at(
                "api.anthropic.com",
                t0 + LEAF_VALIDITY - REMINT_MARGIN + Duration::minutes(1),
            )
            .unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(m.cache_size(), 1);
    }

    /// The old cache had no expiry at all, so a leaf minted once was
    /// served for the life of the process. Past `not_after` there must be
    /// a fresh mint.
    #[test]
    fn expired_cache_entry_is_never_served() {
        let m = CertMint::new(ca());
        let t0 = Utc.with_ymd_and_hms(2026, 8, 7, 9, 0, 0).unwrap();
        let a = m.leaf_for_at("api.anthropic.com", t0).unwrap();
        let b = m
            .leaf_for_at("api.anthropic.com", t0 + LEAF_VALIDITY + Duration::hours(1))
            .unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn rejects_non_ascii_hostnames() {
        // rcgen's SanType::DnsName takes an Ia5String (ASCII subset);
        // anything outside that should fail at conversion.
        let m = CertMint::new(ca());
        assert!(m.leaf_for("api.例え.com").is_err());
    }
}
