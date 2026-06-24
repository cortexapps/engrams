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

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

const CA_COMMON_NAME: &str = "Engram Egress Proxy CA";
const CA_ORGANIZATION: &str = "Engram";

/// Material a `CertMint` needs to sign per-SNI leaves: the CA's
/// key pair (used to sign), CertificateParams (used to rebuild the
/// in-memory issuer cert on each restart), and the PEM bytes the
/// harness substrate stamps into the guest trust store.
///
/// Reload note: on restart we re-self-sign the issuer with the persisted
/// KeyPair. The resulting cert is *not* byte-identical to the one on disk, but
/// its leaves still validate against the delivered cert provided **both the
/// issuer's subject DN and its public key match** the delivered cert. X.509
/// path-building keys on the issuer DN ↔ trusted-CA subject DN; a matching key
/// alone is not enough. So `from_pem` parses params (DN included) from the loaded
/// cert via `CertificateParams::from_ca_cert_pem` rather than rebuilding them from
/// constants — otherwise a deployed CA whose DN differs from the constants signs
/// leaves no guest can verify. Operators bake the persisted cert into the trust
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
        Self::from_pem(&cert_pem, &key_pem)
    }

    /// Build a `Ca` directly from cert+key PEM strings — no disk I/O.
    /// Used by deployments that source the CA from a secret manager
    /// (k8s Secret backed by GCP Secret Manager, etc.) instead of a
    /// per-host local file. Every replica/host loading the same
    /// material produces an issuer with the same SubjectPublicKeyInfo,
    /// so leaves they sign all validate against the cert baked into
    /// guest substrates.
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self, CaError> {
        let key_pair =
            KeyPair::from_pem(key_pem).map_err(|e| CaError::Rcgen(format!("parse ca key: {e}")))?;
        // Parse params — crucially the distinguished name — FROM the loaded cert,
        // so the issuer we re-self-sign to mint leaves (see `cert_mint`) carries
        // the SAME subject DN as the cert delivered to + trusted by guests.
        //
        // Rebuilding params from hardcoded constants instead silently broke TLS
        // validation once a *deployed* CA's DN diverged from the constant (the
        // `Engram`→`Engrams` rename: the secret's CN became `Engrams Egress Proxy
        // CA` with no Organization, while the constant stayed `CN=Engram, O=Engram`).
        // X.509 path-building matches a leaf's ISSUER DN to the trusted CA's SUBJECT
        // DN — a matching public key is necessary but NOT sufficient — so leaves
        // signed under the constant DN failed `unable to get local issuer certificate`
        // against the delivered cert. (rcgen 0.13.2 *does* support this round-trip
        // via `from_ca_cert_pem`; the old "no round-trip" note was stale.)
        let params = CertificateParams::from_ca_cert_pem(cert_pem)
            .map_err(|e| CaError::Rcgen(format!("parse ca cert params: {e}")))?;
        Ok(Self {
            key_pair,
            params,
            cert_pem: cert_pem.to_string(),
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

// ---------------------------------------------------------------------
// CaSource — pluggable backend for loading the deployment CA.
// ---------------------------------------------------------------------

/// Loads the deployment-wide egress-proxy CA.
///
/// Multi-host production needs every host-agent to MITM with the same
/// cert chain — guest substrates are baked with one CA cert and the
/// leaves any host's proxy mints have to validate against it. The
/// abstraction is a trait so cloud-specific backends (GCP Secret
/// Manager, AWS Secrets Manager, Vault) can plug in without
/// `engram-egress-proxy` taking a dep on any of them. ADR 0006.
#[async_trait]
pub trait CaSource: Send + Sync {
    async fn load(&self) -> Result<Ca, CaError>;
}

/// Reads CA cert + key PEM verbatim from two env vars. Suitable for
/// deployments that already project secrets into the process env via
/// some out-of-band mechanism (k8s Secret CSI projection, GCE
/// startup script writing to the systemd unit's Environment, …).
#[derive(Clone, Debug)]
pub struct EnvCaSource {
    pub cert_var: String,
    pub key_var: String,
}

impl EnvCaSource {
    pub fn new(cert_var: impl Into<String>, key_var: impl Into<String>) -> Self {
        Self {
            cert_var: cert_var.into(),
            key_var: key_var.into(),
        }
    }
}

#[async_trait]
impl CaSource for EnvCaSource {
    async fn load(&self) -> Result<Ca, CaError> {
        let cert_pem = std::env::var(&self.cert_var)
            .map_err(|_| CaError::Rcgen(format!("env var `{}` not set", self.cert_var)))?;
        let key_pem = std::env::var(&self.key_var)
            .map_err(|_| CaError::Rcgen(format!("env var `{}` not set", self.key_var)))?;
        Ca::from_pem(&cert_pem, &key_pem)
    }
}

/// Reads (or generates on first run) CA cert + key from a local
/// directory. The dev path: `just dev` boots with no env vars and
/// gets a stable CA across restarts. Not appropriate for stateless
/// multi-host production — each replica would generate its own CA
/// and leaves wouldn't validate cross-host.
#[derive(Clone, Debug)]
pub struct LocalDiskCaSource {
    pub dir: PathBuf,
}

impl LocalDiskCaSource {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

#[async_trait]
impl CaSource for LocalDiskCaSource {
    async fn load(&self) -> Result<Ca, CaError> {
        // `Ca::load_or_generate` is sync but cheap; running it on the
        // current thread is fine. No blocking-pool dispatch needed.
        Ca::load_or_generate(&self.dir)
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;

    #[tokio::test]
    async fn local_disk_source_round_trips_through_load_or_generate() {
        let tmp = tempfile::tempdir().unwrap();
        let source = LocalDiskCaSource::new(tmp.path());
        let ca1 = source.load().await.unwrap();
        let ca2 = source.load().await.unwrap();
        // Same on-disk material → same persisted cert PEM.
        assert_eq!(ca1.cert_pem, ca2.cert_pem);
    }

    #[tokio::test]
    async fn env_source_reports_missing_vars_explicitly() {
        // Pick var names unlikely to clash with any real env var; the
        // test process's actual env is whatever cargo decided.
        let source = EnvCaSource::new(
            "__ENGRAM_TEST_CA_CERT_UNSET__",
            "__ENGRAM_TEST_CA_KEY_UNSET__",
        );
        let err = source
            .load()
            .await
            .err()
            .expect("missing env vars must error");
        match err {
            CaError::Rcgen(msg) => {
                assert!(msg.contains("__ENGRAM_TEST_CA_CERT_UNSET__"));
            }
            other => panic!("expected Rcgen(missing-var), got {other}"),
        }
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

    #[test]
    fn from_pem_round_trips_through_load_or_generate() {
        // Stateless deploy path: one tempdir generates a CA on disk,
        // a second `Ca` constructed via `from_pem` against the same
        // material must produce an issuer with the same key (so a
        // leaf signed by either validates against the on-disk cert).
        let tmp = tempfile::tempdir().unwrap();
        let on_disk = Ca::load_or_generate(tmp.path()).unwrap();
        let key_pem = std::fs::read_to_string(tmp.path().join("ca.key")).unwrap();
        let from_env = Ca::from_pem(&on_disk.cert_pem, &key_pem).unwrap();
        assert_eq!(from_env.cert_pem, on_disk.cert_pem);
        // KeyPair doesn't expose equality, but `serialize_pem`
        // round-trips deterministically.
        assert_eq!(from_env.key_pair.serialize_pem(), key_pem);
    }

    #[test]
    fn from_pem_signs_with_the_loaded_cert_dn_not_the_constants() {
        // A deployed CA whose DN differs from build_params()'s constants — mimics
        // the `Engram`→`Engrams` secret rename (CN only, no Organization). The
        // issuer we re-self-sign to mint leaves must carry THIS DN, so leaves
        // validate against the delivered cert — NOT the hardcoded constant DN.
        let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
        p.distinguished_name = {
            let mut dn = DistinguishedName::new();
            dn.push(DnType::CommonName, "Engrams Egress Proxy CA");
            dn
        };
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let kp = KeyPair::generate().unwrap();
        let cert_pem = p.self_signed(&kp).unwrap().pem();
        let key_pem = kp.serialize_pem();

        let ca = Ca::from_pem(&cert_pem, &key_pem).unwrap();
        let loaded = CertificateParams::from_ca_cert_pem(&cert_pem).unwrap();
        assert_eq!(
            ca.params.distinguished_name, loaded.distinguished_name,
            "issuer DN must come from the loaded cert, not the constants"
        );
        assert_ne!(
            ca.params.distinguished_name,
            build_params().distinguished_name,
            "regression guard: issuer DN must NOT be the hardcoded constants"
        );
    }

    #[test]
    fn from_pem_rejects_malformed_key() {
        let result = Ca::from_pem(
            "-----BEGIN CERTIFICATE-----\nMII...\n-----END CERTIFICATE-----\n",
            "not-a-key",
        );
        assert!(matches!(result, Err(CaError::Rcgen(_))));
    }
}
