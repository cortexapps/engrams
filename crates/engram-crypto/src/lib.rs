//! Envelope encryption for Engram-managed secrets at rest.
//!
//! # Threat model
//!
//! We protect deployment-level credentials (Docker registry passwords,
//! later: external API tokens) stored in Postgres. The Postgres row
//! holds ciphertext only; the master key (KEK) lives outside the
//! database — in an env var (sourced from k8s Secrets / Doppler / etc.)
//! or in a KMS. A read of the Postgres row alone does not yield
//! plaintext.
//!
//! # Layout
//!
//! Envelope: per-secret AES-256-GCM data key (DEK), wrapped by the
//! KEK. The KEK never touches plaintext secrets directly — it only
//! wraps DEKs. This gives us:
//!
//! - Cheap key rotation: rotate KEK, rewrap DEKs lazily on next read.
//! - Bounded KMS calls: one wrap per write, one unwrap per read; each
//!   wrap/unwrap operates on a 32-byte DEK regardless of plaintext
//!   size.
//! - Forward-compatibility: when we want to encrypt larger blobs (e.g.
//!   SSH keys, JWT signers), the same shape applies.
//!
//! # Wire format
//!
//! `SealedCred` is `(wrapped_dek, nonce, ciphertext, key_id)`. The
//! Postgres schema stores them as four columns rather than one packed
//! BYTEA so they're individually queryable / auditable.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key};
use async_trait::async_trait;
use rand::rngs::SysRng;
use rand::TryRng;

pub mod providers;

pub use providers::{EnvVarKeyProvider, GcpKmsProvider};

/// Wraps and unwraps data encryption keys (DEKs) using a master key.
///
/// Implementations are responsible for the master key's lifecycle:
/// `EnvVarKeyProvider` reads from process env (key never persists past
/// the process); `GcpKmsProvider` calls KMS so the master key never
/// leaves the KMS at all.
#[async_trait]
pub trait MasterKeyProvider: Send + Sync {
    /// Wrap a 32-byte DEK. The returned bytes are opaque — the
    /// provider decides the format.
    async fn wrap(&self, dek: &[u8]) -> Result<Vec<u8>, CryptoError>;

    /// Unwrap a previously-wrapped DEK. Errors if the wrapped value
    /// was produced by a different KEK or a different provider, or if
    /// `key_id()` no longer matches what produced it (the caller can
    /// detect rotation by comparing the row's `key_id` against this
    /// method).
    async fn unwrap(&self, wrapped: &[u8]) -> Result<Vec<u8>, CryptoError>;

    /// Stable identifier for the active KEK. Stored alongside each
    /// SealedCred so we can detect rotation and trigger lazy rewrap.
    /// Examples: `env:ENGRAM_KEK_MASTER_KEY:v1`,
    /// `gcp-kms:projects/p/locations/global/keyRings/r/cryptoKeys/k:7`.
    fn key_id(&self) -> &str;
}

/// Sealed credential: AES-256-GCM ciphertext + the wrapped DEK that
/// produced it + the nonce used + the KEK identifier in effect at
/// seal time.
#[derive(Clone, Debug)]
pub struct SealedCred {
    pub wrapped_dek: Vec<u8>,
    pub nonce: [u8; 12],
    pub ciphertext: Vec<u8>,
    pub key_id: String,
}

/// Wraps a `MasterKeyProvider` to seal and open arbitrary byte
/// slices. The cipher itself is stateless; all the per-secret state
/// lives in the returned `SealedCred`.
pub struct CredCipher<'a> {
    pub kek: &'a dyn MasterKeyProvider,
}

impl<'a> CredCipher<'a> {
    pub fn new(kek: &'a dyn MasterKeyProvider) -> Self {
        Self { kek }
    }

    /// Generate a fresh DEK + nonce, encrypt `plaintext` with
    /// AES-256-GCM, wrap the DEK with the KEK, and tag the result
    /// with the active KEK's `key_id`.
    pub async fn seal(&self, plaintext: &[u8]) -> Result<SealedCred, CryptoError> {
        // SysRng is `Send` (unit struct around the OS RNG). We avoid
        // `rand::rng()` here because it returns a `!Send` handle,
        // which would poison the future with `!Send` and break axum
        // handlers that hold a `CredCipher` across an await point.
        //
        // rand 0.10 makes the OS RNG explicitly fallible: SysRng
        // implements only `TryRng`, where 0.9's `OsRng` implemented
        // the infallible `RngCore` and panicked internally on a failed
        // syscall. Propagate instead — a DEK or nonce must never come
        // from a degraded source, and both callers already handle
        // `CryptoError`.
        let mut rng = SysRng;

        let mut dek = [0u8; 32];
        rng.try_fill_bytes(&mut dek)
            .map_err(|e| CryptoError::Provider(format!("OS RNG failed for DEK: {e}")))?;
        let mut nonce = [0u8; 12];
        rng.try_fill_bytes(&mut nonce)
            .map_err(|e| CryptoError::Provider(format!("OS RNG failed for nonce: {e}")))?;

        let cipher = Aes256Gcm::new((&dek).into());
        let ciphertext = cipher
            .encrypt((&nonce).into(), plaintext)
            .map_err(|_| CryptoError::Aead("encrypt"))?;

        let wrapped_dek = self.kek.wrap(&dek).await?;

        Ok(SealedCred {
            wrapped_dek,
            nonce,
            ciphertext,
            key_id: self.kek.key_id().to_string(),
        })
    }

    /// Unwrap the DEK and decrypt the ciphertext. Errors if the DEK
    /// can't be unwrapped (wrong KEK, corrupted bytes) or the GCM tag
    /// doesn't authenticate (tampered ciphertext / nonce).
    pub async fn open(&self, sealed: &SealedCred) -> Result<Vec<u8>, CryptoError> {
        let dek = self.kek.unwrap(&sealed.wrapped_dek).await?;
        // A DEK that is not exactly 32 bytes means the wrong KEK or a wrap
        // format mismatch. `TryFrom` is the length check — it is the same
        // condition the explicit comparison used to test.
        let key =
            <&Key<Aes256Gcm>>::try_from(&dek[..]).map_err(|_| CryptoError::DekLength(dek.len()))?;
        let cipher = Aes256Gcm::new(key);
        cipher
            .decrypt((&sealed.nonce).into(), sealed.ciphertext.as_slice())
            .map_err(|_| CryptoError::Aead("decrypt"))
    }
}

#[derive(Debug)]
pub enum CryptoError {
    /// AES-GCM seal/open failed (auth tag mismatch, etc.).
    Aead(&'static str),
    /// Unwrapped DEK was not 32 bytes — wrong KEK, or wrap format mismatch.
    DekLength(usize),
    /// Provider-specific failure (env var missing, KMS API error, ...).
    Provider(String),
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Aead(op) => write!(f, "AEAD {op} failed"),
            Self::DekLength(n) => write!(f, "unwrapped DEK has wrong length ({n}, expected 32)"),
            Self::Provider(s) => write!(f, "KEK provider error: {s}"),
        }
    }
}

impl std::error::Error for CryptoError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a compile-time hex literal into a byte vector.
    fn unhex(s: &str) -> Vec<u8> {
        assert!(
            s.len().is_multiple_of(2),
            "hex literal must have even length"
        );
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    /// A `SealedCred` produced by aes-gcm **0.10.3** — the version this
    /// repo shipped before the 0.11 bump — must still open.
    ///
    /// Every other test in this module seals and opens with the same code,
    /// so all of them would keep passing if an upgrade silently changed the
    /// envelope's wire format. Rows already sealed in Postgres would then be
    /// permanently unreadable, and nothing would say so. This test is the
    /// only thing standing between that failure and a green build.
    ///
    /// The bytes below were generated by running the 0.10.3 encrypt path
    /// against fixed inputs (KEK `00..1f`, DEK `42*32`, wrap nonce `11*12`,
    /// DEK nonce `22*12`). Treat a failure here the way you would treat a
    /// content-addressing digest failure: it is a data-migration problem,
    /// never a new expected value to paste in.
    #[tokio::test]
    async fn opens_an_envelope_sealed_by_aes_gcm_0_10_3() {
        let kek = EnvVarKeyProvider::from_bytes(
            [
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
                0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
                0x1c, 0x1d, 0x1e, 0x1f,
            ],
            "test:v1",
        );

        let sealed = SealedCred {
            wrapped_dek: unhex(
                "111111111111111111111111c222d6fdfa426dec2a3d6740036752f8a15dc2e7\
                 5773d22b6f99d9446a218110dc6069e1532af4924568258f9e1b5796",
            ),
            nonce: [0x22; 12],
            ciphertext: unhex(
                "5377c6bb87fb28e4e5d49bbc5d074103eeb580390e308c58a537d42a7c8d65d0\
                 b782f1687217589da8",
            ),
            key_id: "test:v1".to_string(),
        };

        let opened = CredCipher::new(&kek).open(&sealed).await.unwrap();
        assert_eq!(
            opened, b"hunter2:registry-password",
            "aes-gcm 0.11 must decrypt an envelope written by 0.10.3"
        );
    }

    #[tokio::test]
    async fn seal_open_round_trip() {
        let kek = EnvVarKeyProvider::from_bytes(*b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x10\x11\x12\x13\x14\x15\x16\x17\x18\x19\x1a\x1b\x1c\x1d\x1e\x1f", "test:v1");
        let cipher = CredCipher::new(&kek);

        let sealed = cipher.seal(b"hunter2").await.unwrap();
        assert_eq!(sealed.key_id, "test:v1");
        assert_ne!(
            &sealed.ciphertext[..],
            b"hunter2",
            "ciphertext must not equal plaintext"
        );

        let opened = cipher.open(&sealed).await.unwrap();
        assert_eq!(opened, b"hunter2");
    }

    #[tokio::test]
    async fn each_seal_uses_a_fresh_dek_and_nonce() {
        let kek = EnvVarKeyProvider::from_bytes([0u8; 32], "test:v1");
        let cipher = CredCipher::new(&kek);
        let a = cipher.seal(b"same plaintext").await.unwrap();
        let b = cipher.seal(b"same plaintext").await.unwrap();
        // Different DEK → different wrapped_dek; different nonce → different ciphertext.
        assert_ne!(a.wrapped_dek, b.wrapped_dek);
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[tokio::test]
    async fn tampered_ciphertext_fails_to_open() {
        let kek = EnvVarKeyProvider::from_bytes([1u8; 32], "test:v1");
        let cipher = CredCipher::new(&kek);
        let mut sealed = cipher.seal(b"important").await.unwrap();
        // Flip a bit.
        sealed.ciphertext[0] ^= 0x01;
        assert!(matches!(
            cipher.open(&sealed).await,
            Err(CryptoError::Aead(_))
        ));
    }

    #[tokio::test]
    async fn wrong_kek_fails_to_open() {
        let kek_a = EnvVarKeyProvider::from_bytes([0xaa; 32], "a:v1");
        let kek_b = EnvVarKeyProvider::from_bytes([0xbb; 32], "b:v1");
        let cipher_a = CredCipher::new(&kek_a);
        let cipher_b = CredCipher::new(&kek_b);

        let sealed = cipher_a.seal(b"secret").await.unwrap();
        // Different KEK → DEK unwrap yields garbage → AEAD open fails.
        let err = cipher_b.open(&sealed).await.unwrap_err();
        assert!(matches!(
            err,
            CryptoError::Aead(_) | CryptoError::DekLength(_)
        ));
    }
}
