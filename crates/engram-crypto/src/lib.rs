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
use aes_gcm::{Aes256Gcm, Key, Nonce};
use async_trait::async_trait;
use rand::rngs::OsRng;
use rand::RngCore;

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
        // OsRng is `Send` (unit struct around the OS RNG). We avoid
        // `thread_rng()` here because it returns a `!Send` handle,
        // which would poison the future with `!Send` and break axum
        // handlers that hold a `CredCipher` across an await point.
        let mut rng = OsRng;

        let mut dek = [0u8; 32];
        rng.fill_bytes(&mut dek);
        let mut nonce = [0u8; 12];
        rng.fill_bytes(&mut nonce);

        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&dek));
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext)
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
        if dek.len() != 32 {
            return Err(CryptoError::DekLength(dek.len()));
        }
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&dek));
        cipher
            .decrypt(
                Nonce::from_slice(&sealed.nonce),
                sealed.ciphertext.as_slice(),
            )
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
