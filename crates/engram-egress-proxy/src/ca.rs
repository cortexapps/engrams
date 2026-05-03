//! CA load-or-generate + persistence.
//!
//! One CA per host-agent install. Persisted to
//! `<work_dir>/ca.pem` + `<work_dir>/ca.key`. Survives coord restarts
//! so the same baked-into-substrate cert stays valid across reboots
//! — otherwise every restart would invalidate every running session's
//! trust-store install.
//!
//! Per-sandbox CAs would give slightly tighter isolation (a
//! compromised sandbox can't forge its own leaf without our signing
//! key, but per-host means all sandboxes' leaves chain to the same
//! root). For our threat model — guests are untrusted, host is the
//! TCB — per-host is fine: a sandbox can't ever obtain the signing
//! key in either model, since the proxy lives on the host.

use std::path::Path;

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

const CA_COMMON_NAME: &str = "Engram Egress Proxy CA";
const CA_ORGANIZATION: &str = "Engram";

/// Material a `CertMint` needs to sign per-SNI leaves: the CA's
/// key pair (used to sign), CertificateParams (used to rebuild the
/// in-memory issuer cert on each restart), and the PEM bytes the
/// harness substrate stamps into the guest trust store.
///
/// Reload note: rcgen 0.13 has no round-trip from PEM back to
/// `CertificateParams`, so on restart we rebuild params from
/// constants and re-self-sign with the persisted KeyPair. The
/// resulting cert is *not* byte-identical to the one on disk, but
/// the leaves it signs still validate against the on-disk PEM
/// because trust validation only requires the issuer's
/// SubjectPublicKeyInfo (the key) to match — and the key IS
/// persisted. Operators bake the persisted cert into the trust
/// store; the in-memory cert is only used to sign new leaves.
pub struct Ca {
    pub key_pair: KeyPair,
    pub params: CertificateParams,
    /// Cert PEM persisted to disk on first startup. Stable across
    /// restarts — this is what gets baked into the harness substrate
    /// so existing sandboxes' trust stores stay valid.
    pub cert_pem: String,
}

impl Ca {
    /// Load from `dir` if both `ca.pem` + `ca.key` exist, otherwise
    /// generate a fresh CA and write both files. Idempotent across
    /// restarts.
    pub fn load_or_generate(dir: &Path) -> Result<Self, CaError> {
        let cert_path = dir.join("ca.pem");
        let key_path = dir.join("ca.key");
        if cert_path.exists() && key_path.exists() {
            return Self::load(&cert_path, &key_path);
        }
        std::fs::create_dir_all(dir)?;
        Self::generate_and_persist(&cert_path, &key_path)
    }

    fn load(cert_path: &Path, key_path: &Path) -> Result<Self, CaError> {
        let cert_pem = std::fs::read_to_string(cert_path)?;
        let key_pem = std::fs::read_to_string(key_path)?;
        let key_pair = KeyPair::from_pem(&key_pem)
            .map_err(|e| CaError::Rcgen(format!("parse ca key: {e}")))?;
        // rcgen 0.13 has no Cert→Params round-trip, so we rebuild
        // params from constants. Validity in the rebuilt params is
        // independent of the persisted cert; what matters is that
        // the SubjectPublicKeyInfo (= key_pair) matches.
        let params = build_params();
        Ok(Self {
            key_pair,
            params,
            cert_pem,
        })
    }

    fn generate_and_persist(cert_path: &Path, key_path: &Path) -> Result<Self, CaError> {
        let params = build_params();

        let key_pair =
            KeyPair::generate().map_err(|e| CaError::Rcgen(format!("ca keygen: {e}")))?;
        let cert = params
            .clone()
            .self_signed(&key_pair)
            .map_err(|e| CaError::Rcgen(format!("ca self-sign: {e}")))?;
        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        // mode 0600 on the key — the CA private key is the most
        // sensitive thing on the host; treat it like an SSH key.
        write_secret(key_path, &key_pem)?;
        std::fs::write(cert_path, &cert_pem)?;
        Ok(Self {
            key_pair,
            params,
            cert_pem,
        })
    }
}

fn build_params() -> CertificateParams {
    let mut params = CertificateParams::new(Vec::<String>::new())
        .expect("CertificateParams::new with empty SANs is infallible");
    params.distinguished_name = {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, CA_COMMON_NAME);
        dn.push(DnType::OrganizationName, CA_ORGANIZATION);
        dn
    };
    // 10 years — sandboxes are ephemeral but the CA is host-side
    // and we don't want surprise expiry to break sessions.
    use chrono::Datelike;
    let now = chrono::Utc::now();
    let later = now + chrono::Duration::days(3650);
    params.not_before = rcgen::date_time_ymd(now.year(), now.month() as u8, now.day() as u8);
    params.not_after = rcgen::date_time_ymd(later.year(), later.month() as u8, later.day() as u8);
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    params
}

fn write_secret(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

#[derive(Debug)]
pub enum CaError {
    Io(std::io::Error),
    Rcgen(String),
}

impl std::fmt::Display for CaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Rcgen(s) => write!(f, "rcgen: {s}"),
        }
    }
}

impl std::error::Error for CaError {}

impl From<std::io::Error> for CaError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_writes_both_files_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_generate(tmp.path()).unwrap();
        assert!(tmp.path().join("ca.pem").exists());
        assert!(tmp.path().join("ca.key").exists());
        // Re-load from the same dir — should not regenerate.
        let cert_pem_before = ca.cert_pem.clone();
        let ca2 = Ca::load_or_generate(tmp.path()).unwrap();
        assert_eq!(ca2.cert_pem, cert_pem_before);
    }

    #[test]
    fn key_file_is_mode_600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        Ca::load_or_generate(tmp.path()).unwrap();
        let mode = std::fs::metadata(tmp.path().join("ca.key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn persisted_pem_starts_with_begin_certificate() {
        let tmp = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_generate(tmp.path()).unwrap();
        assert!(ca.cert_pem.contains("-----BEGIN CERTIFICATE-----"));
    }
}
