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
//! Each leaf is valid for ~1 day. Short enough that a key
//! compromise has limited blast radius; long enough that we don't
//! re-mint on every connection.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use rustls::crypto::ring::sign::any_supported_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;

use crate::ca::Ca;

pub struct CertMint {
    ca: Arc<Ca>,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
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
        let key = hostname.to_ascii_lowercase();
        if let Some(c) = self.cache.lock().get(&key) {
            return Ok(c.clone());
        }
        let minted = self.mint(&key)?;
        self.cache.lock().insert(key, minted.clone());
        Ok(minted)
    }

    fn mint(&self, hostname: &str) -> Result<Arc<CertifiedKey>, MintError> {
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
        // 25h validity — slightly over a day so we don't churn at
        // exactly the same wall-clock time tomorrow.
        use chrono::Datelike;
        let now = crate::time_source::wall_now();
        let earlier = now - chrono::Duration::minutes(5);
        let later = now + chrono::Duration::hours(25);
        params.not_before =
            rcgen::date_time_ymd(earlier.year(), earlier.month() as u8, earlier.day() as u8);
        params.not_after =
            rcgen::date_time_ymd(later.year(), later.month() as u8, later.day() as u8);

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
        Ok(Arc::new(CertifiedKey::new(vec![cert], signing_key)))
    }

    pub fn cache_size(&self) -> usize {
        self.cache.lock().len()
    }
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

    #[test]
    fn rejects_non_ascii_hostnames() {
        // rcgen's SanType::DnsName takes an Ia5String (ASCII subset);
        // anything outside that should fail at conversion.
        let m = CertMint::new(ca());
        assert!(m.leaf_for("api.例え.com").is_err());
    }
}
