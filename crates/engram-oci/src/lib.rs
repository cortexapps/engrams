//! OCI Distribution Spec client for Engram-managed artifacts.
//!
//! Two artifact families:
//!
//! 1. **Bake images** — pre-baked rootfs.ext4 + manifest.toml. Two
//!    layers, custom mediaTypes; host-agents pull and use the layer
//!    bytes directly (no Docker daemon required).
//! 2. **Harness packs** — directory of executable + sidecars, tarred
//!    into a single layer.
//!
//! Both push to and pull from any standard OCI Distribution registry
//! (GHCR, GCR, ECR, Harbor, local `registry:2`). Auth is resolved via
//! the [`RegistryAuthResolver`] trait — the coordinator wires this to
//! its encrypted Postgres registry-credential store; tests / the host
//! agent's anonymous-only path use [`AnonymousResolver`].
//!
//! ## HTTP fallback for local registries
//!
//! Standard OCI clients reject plaintext HTTP for security. We allow
//! it only when the registry host is `localhost`, `127.0.0.1`, or
//! `::1` — same heuristic Docker uses for `--insecure-registry`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use oci_client::client::{ClientConfig, ClientProtocol, Config, ImageLayer, PushResponse};
use oci_client::manifest::OCI_IMAGE_MEDIA_TYPE;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub mod media_types;

pub use media_types::*;

/// Resolves auth credentials for a given registry host.
///
/// Returning `None` is the anonymous-pull / unauthenticated path —
/// works for public registries (`docker.io/library/...`) and the
/// local dev `registry:2` (no auth configured).
#[async_trait]
pub trait RegistryAuthResolver: Send + Sync {
    async fn resolve(&self, registry_host: &str) -> Result<Option<BasicCreds>, OciError>;
}

#[derive(Clone, Debug)]
pub struct BasicCreds {
    pub username: String,
    pub password: String,
}

/// Resolver that always returns `None`. Used in dev when no creds are
/// configured, in tests, and as a fallback when the metadata-store
/// resolver finds no row for the host.
pub struct AnonymousResolver;

#[async_trait]
impl RegistryAuthResolver for AnonymousResolver {
    async fn resolve(&self, _registry_host: &str) -> Result<Option<BasicCreds>, OciError> {
        Ok(None)
    }
}

/// Wraps `oci-client::Client` with Engram-specific push/pull verbs
/// and credential resolution.
#[derive(Clone)]
pub struct OciClient {
    inner: Client,
    auth: Arc<dyn RegistryAuthResolver>,
}

impl OciClient {
    pub fn new(auth: Arc<dyn RegistryAuthResolver>) -> Self {
        // We can't know per-call which registries are HTTP vs HTTPS at
        // construction time (the user passes URIs as strings). Set
        // `Https` as the default; we re-pick the protocol per push/pull
        // by reconstructing the client config when the host looks
        // plaintext-eligible (loopback). This is mildly inefficient but
        // simple — registries are pulled from infrequently relative to
        // session creation throughput.
        let inner = Client::new(ClientConfig {
            protocol: ClientProtocol::Https,
            ..Default::default()
        });
        Self { inner, auth }
    }

    /// Build a client whose protocol matches `reference`'s registry
    /// host: `Http` for loopback, `Https` otherwise. Used per-operation
    /// because plaintext is the wrong default everywhere except dev.
    fn client_for(reference: &Reference) -> Client {
        let proto = if is_loopback_host(reference.registry()) {
            ClientProtocol::Http
        } else {
            ClientProtocol::Https
        };
        Client::new(ClientConfig {
            protocol: proto,
            ..Default::default()
        })
    }

    async fn auth_for(&self, reference: &Reference) -> Result<RegistryAuth, OciError> {
        match self.auth.resolve(reference.registry()).await? {
            Some(c) => Ok(RegistryAuth::Basic(c.username, c.password)),
            None => Ok(RegistryAuth::Anonymous),
        }
    }

    /// Push a bake image artifact. Two layers:
    ///
    /// - layer 0: `manifest.toml` content (uncompressed bytes)
    /// - layer 1: `rootfs.ext4` content (raw bytes, large)
    ///
    /// `config_json` is small JSON metadata (format, agent version,
    /// transport) used by the host-agent at pull time to validate
    /// compatibility before fetching the rootfs blob.
    pub async fn push_image(
        &self,
        uri: &str,
        manifest_toml: &[u8],
        rootfs_ext4: &Path,
        config_json: &[u8],
    ) -> Result<Digest256, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = Self::client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        let manifest_layer = ImageLayer::new(
            manifest_toml.to_vec(),
            ENGRAM_MANIFEST_MEDIA_TYPE.to_string(),
            None,
        );
        let rootfs_bytes = read_file_bytes(rootfs_ext4).await?;
        let rootfs_layer = ImageLayer::new(
            rootfs_bytes,
            ENGRAM_ROOTFS_EXT4_MEDIA_TYPE.to_string(),
            None,
        );
        let layers = vec![manifest_layer, rootfs_layer];

        let config = Config::new(
            config_json.to_vec(),
            ENGRAM_IMAGE_CONFIG_MEDIA_TYPE.to_string(),
            None,
        );

        let resp: PushResponse = client
            .push(&reference, &layers, config, &auth, None)
            .await
            .map_err(|e| OciError::Distribution(e.to_string()))?;
        // PushResponse exposes the manifest_url; we want the digest of
        // the manifest. oci-client computes it on push and returns it
        // in the response.
        Ok(Digest256(resp.manifest_url))
    }

    /// Pull a bake image artifact. Writes
    /// `<dest>/manifest.toml` and `<dest>/rootfs.ext4`. Returns the
    /// digest of the OCI manifest (a content address — a re-pull of
    /// the same tag with unchanged layers yields the same digest).
    pub async fn pull_image(&self, uri: &str, dest: &Path) -> Result<PulledImage, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = Self::client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        let accepted = vec![
            ENGRAM_MANIFEST_MEDIA_TYPE,
            ENGRAM_ROOTFS_EXT4_MEDIA_TYPE,
            OCI_IMAGE_MEDIA_TYPE,
        ];
        let data = client
            .pull(&reference, &auth, accepted)
            .await
            .map_err(|e| OciError::Distribution(e.to_string()))?;

        tokio::fs::create_dir_all(dest)
            .await
            .map_err(OciError::Io)?;

        let mut manifest_path = None;
        let mut rootfs_path = None;
        for layer in &data.layers {
            match layer.media_type.as_str() {
                ENGRAM_MANIFEST_MEDIA_TYPE => {
                    let p = dest.join("manifest.toml");
                    write_file_bytes(&p, &layer.data).await?;
                    manifest_path = Some(p);
                }
                ENGRAM_ROOTFS_EXT4_MEDIA_TYPE => {
                    let p = dest.join("rootfs.ext4");
                    write_file_bytes(&p, &layer.data).await?;
                    rootfs_path = Some(p);
                }
                other => {
                    tracing::debug!(media_type = %other, "skipping unrecognized layer");
                }
            }
        }
        let manifest_path = manifest_path.ok_or_else(|| {
            OciError::Distribution("pulled artifact missing engram manifest layer".into())
        })?;
        let rootfs_path = rootfs_path.ok_or_else(|| {
            OciError::Distribution("pulled artifact missing engram rootfs.ext4 layer".into())
        })?;

        Ok(PulledImage {
            manifest_path,
            rootfs_path,
            manifest_digest: Digest256(data.digest.unwrap_or_default()),
        })
    }

    /// Fetch only the engram manifest.toml layer of an engram image
    /// artifact, plus its top-level manifest digest. Skips the
    /// rootfs.ext4 layer entirely — the registry's `pull` returns the
    /// full layer set, but we drop the rootfs blob without writing it.
    /// Returns `(manifest_bytes, manifest_digest)`.
    ///
    /// Used by the coordinator's `/api/enabled-images` POST handler:
    /// when an operator enables an image, we cache its parsed
    /// manifest on the row so session-create has zero registry I/O.
    /// `oci-distribution`'s `pull` is a single round-trip for the
    /// index + all layer blobs, so we still pay one fetch for the
    /// rootfs bytes — but we don't write them anywhere, and on a
    /// public registry that's a wash. The savings vs. a full
    /// `pull_image()` are storage (no rootfs.ext4 file written) and
    /// cleanup (no temp dir to manage).
    pub async fn pull_engram_manifest_only(
        &self,
        uri: &str,
    ) -> Result<(Vec<u8>, Digest256), OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = Self::client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        let accepted = vec![
            ENGRAM_MANIFEST_MEDIA_TYPE,
            ENGRAM_ROOTFS_EXT4_MEDIA_TYPE,
            OCI_IMAGE_MEDIA_TYPE,
        ];
        let data = client
            .pull(&reference, &auth, accepted)
            .await
            .map_err(|e| OciError::Distribution(e.to_string()))?;

        let manifest_layer = data
            .layers
            .iter()
            .find(|l| l.media_type == ENGRAM_MANIFEST_MEDIA_TYPE)
            .ok_or_else(|| {
                OciError::Distribution("pulled artifact missing engram manifest layer".into())
            })?;

        Ok((
            manifest_layer.data.clone(),
            Digest256(data.digest.unwrap_or_default()),
        ))
    }

    /// Push a harness pack artifact. Tars + gzips `pack_dir` and pushes
    /// it as a single layer.
    pub async fn push_harness(&self, uri: &str, pack_dir: &Path) -> Result<Digest256, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = Self::client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        let tar_gz = tar_gz_dir(pack_dir).await?;
        let layer = ImageLayer::new(tar_gz, ENGRAM_HARNESS_TAR_MEDIA_TYPE.to_string(), None);
        let config = Config::new(
            br#"{"kind":"engram-harness-v1"}"#.to_vec(),
            ENGRAM_HARNESS_CONFIG_MEDIA_TYPE.to_string(),
            None,
        );

        let resp = client
            .push(&reference, &[layer], config, &auth, None)
            .await
            .map_err(|e| OciError::Distribution(e.to_string()))?;
        Ok(Digest256(resp.manifest_url))
    }

    /// Pull a harness pack artifact and extract it into `dest`. The
    /// caller is responsible for `dest` being empty; we don't clear
    /// pre-existing files.
    pub async fn pull_harness(&self, uri: &str, dest: &Path) -> Result<PulledHarness, OciError> {
        let reference: Reference = uri
            .parse()
            .map_err(|e: oci_client::ParseError| OciError::InvalidUri(format!("{uri}: {e}")))?;
        let client = Self::client_for(&reference);
        let auth = self.auth_for(&reference).await?;

        let accepted = vec![ENGRAM_HARNESS_TAR_MEDIA_TYPE, OCI_IMAGE_MEDIA_TYPE];
        let data = client
            .pull(&reference, &auth, accepted)
            .await
            .map_err(|e| OciError::Distribution(e.to_string()))?;

        tokio::fs::create_dir_all(dest)
            .await
            .map_err(OciError::Io)?;

        let layer = data
            .layers
            .iter()
            .find(|l| l.media_type == ENGRAM_HARNESS_TAR_MEDIA_TYPE)
            .ok_or_else(|| {
                OciError::Distribution("pulled artifact missing engram harness tar layer".into())
            })?;

        untar_gz_to_dir(&layer.data, dest).await?;

        Ok(PulledHarness {
            pack_dir: dest.to_path_buf(),
            manifest_digest: Digest256(data.digest.unwrap_or_default()),
        })
    }

    /// Round-trip token for the suppress-unused-warning. Useful when
    /// embedding `OciClient` in a service that may not exercise it on
    /// some code paths.
    pub fn inner(&self) -> &Client {
        &self.inner
    }
}

/// `sha256:...` content digest of an OCI manifest.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Digest256(pub String);

impl Digest256 {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// Strip the `sha256:` prefix and return the hex bytes. Returns
    /// the original string when no recognized prefix is present.
    pub fn hex(&self) -> &str {
        self.0.strip_prefix("sha256:").unwrap_or(&self.0)
    }
}

#[derive(Clone, Debug)]
pub struct PulledImage {
    pub manifest_path: PathBuf,
    pub rootfs_path: PathBuf,
    pub manifest_digest: Digest256,
}

#[derive(Clone, Debug)]
pub struct PulledHarness {
    pub pack_dir: PathBuf,
    pub manifest_digest: Digest256,
}

#[derive(Debug)]
pub enum OciError {
    InvalidUri(String),
    Distribution(String),
    Io(std::io::Error),
}

impl std::fmt::Display for OciError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUri(s) => write!(f, "invalid OCI URI: {s}"),
            Self::Distribution(s) => write!(f, "OCI distribution error: {s}"),
            Self::Io(e) => write!(f, "OCI io: {e}"),
        }
    }
}

impl std::error::Error for OciError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for OciError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ---- helpers ----

fn is_loopback_host(host: &str) -> bool {
    // The registry portion may include a port. Hostnames and IPv4
    // addresses use `host:port`; IPv6 uses `[addr]:port`. Strip the
    // port if present, then compare.
    let stripped = if let Some(rest) = host.strip_prefix('[') {
        // [::1] or [::1]:5000
        rest.split(']').next().unwrap_or(rest)
    } else if host.matches(':').count() == 1 {
        // host:port (hostname or IPv4) — split off the port
        host.split(':').next().unwrap_or(host)
    } else {
        // bare IPv6 address with no port (e.g. "::1")
        host
    };
    matches!(stripped, "localhost" | "127.0.0.1" | "::1")
}

async fn read_file_bytes(path: &Path) -> Result<Vec<u8>, OciError> {
    let mut f = tokio::fs::File::open(path).await.map_err(OciError::Io)?;
    let meta = f.metadata().await.map_err(OciError::Io)?;
    let mut buf = Vec::with_capacity(meta.len() as usize);
    f.read_to_end(&mut buf).await.map_err(OciError::Io)?;
    Ok(buf)
}

async fn write_file_bytes(path: &Path, bytes: &[u8]) -> Result<(), OciError> {
    let mut f = tokio::fs::File::create(path).await.map_err(OciError::Io)?;
    f.write_all(bytes).await.map_err(OciError::Io)?;
    f.flush().await.map_err(OciError::Io)?;
    Ok(())
}

async fn tar_gz_dir(dir: &Path) -> Result<Vec<u8>, OciError> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>, std::io::Error> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut buf = Vec::new();
        {
            let enc = GzEncoder::new(&mut buf, Compression::default());
            let mut tar = tar::Builder::new(enc);
            tar.append_dir_all(".", &dir)?;
            tar.finish()?;
        }
        Ok(buf)
    })
    .await
    .map_err(|e| OciError::Distribution(format!("join error: {e}")))?
    .map_err(OciError::Io)
}

async fn untar_gz_to_dir(bytes: &[u8], dest: &Path) -> Result<(), OciError> {
    let bytes = bytes.to_vec();
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(), std::io::Error> {
        use flate2::read::GzDecoder;
        let dec = GzDecoder::new(std::io::Cursor::new(bytes));
        let mut tar = tar::Archive::new(dec);
        tar.set_preserve_permissions(true);
        tar.unpack(&dest)
    })
    .await
    .map_err(|e| OciError::Distribution(format!("join error: {e}")))?
    .map_err(OciError::Io)
}

/// Compute the sha256 digest of `bytes` and format as `sha256:hex`.
pub fn sha256_digest(bytes: &[u8]) -> Digest256 {
    let mut h = Sha256::new();
    h.update(bytes);
    Digest256(format!("sha256:{:x}", h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_host_recognized() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("localhost:5000"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.1:5000"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("[::1]:5000"));
        assert!(!is_loopback_host("gcr.io"));
        assert!(!is_loopback_host("registry.example.com:5000"));
        assert!(!is_loopback_host("10.0.0.5:5000"));
    }

    #[test]
    fn sha256_digest_is_stable() {
        let d1 = sha256_digest(b"hello");
        let d2 = sha256_digest(b"hello");
        assert_eq!(d1, d2);
        assert!(d1.0.starts_with("sha256:"));
        assert_eq!(d1.hex().len(), 64);
    }

    #[tokio::test]
    async fn anonymous_resolver_returns_none() {
        let r = AnonymousResolver;
        assert!(r.resolve("anything").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tar_gz_round_trip_preserves_files() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("harness"), b"#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(src.path().join("README"), b"a harness pack").unwrap();

        let tar_bytes = tar_gz_dir(src.path()).await.unwrap();
        let dst = tempfile::tempdir().unwrap();
        untar_gz_to_dir(&tar_bytes, dst.path()).await.unwrap();

        assert_eq!(
            std::fs::read(dst.path().join("harness")).unwrap(),
            b"#!/bin/sh\necho hi\n"
        );
        assert_eq!(
            std::fs::read(dst.path().join("README")).unwrap(),
            b"a harness pack"
        );
    }
}
