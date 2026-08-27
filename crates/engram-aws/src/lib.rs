//! The workspace's AWS SDK bootstrap seam (ADR 0122).
//!
//! One job: every `SdkConfig` in the workspace is constructed HERE, on
//! the workspace's single rustls `ring` CryptoProvider, with the
//! transport posture the blob tier already proved out for GCS. This is
//! the `engram-tls` pattern applied to the AWS SDK: a bootstrap every
//! call site must remember is a latent production panic, so the crate
//! owns construction instead.
//!
//! # Why this crate exists
//!
//! The AWS SDK's default HTTPS client (`default-https-client` on
//! `aws-config` and every `aws-sdk-*` service crate) links `aws-lc-rs`.
//! That is fatal here twice over:
//!
//! - The workspace compiles exactly one rustls CryptoProvider — `ring`,
//!   installed process-wide by `engram-tls`. A second compiled provider
//!   makes `CryptoProvider::get_default()` ambiguous and rustls panics
//!   at first TLS use.
//! - `aws-lc-rs` is a cmake+nasm C/asm build the
//!   `aarch64-unknown-linux-musl` cross lane cannot carry.
//!
//! So: every AWS dependency is `default-features = false`, TLS comes
//! exclusively from `aws-smithy-http-client`'s `rustls-ring`, and CI
//! greps `Cargo.lock` so `aws-lc` entering the tree is a deterministic
//! build failure instead of a runtime panic.
//!
//! # Transport posture
//!
//! Mirrors the measured GCS client in `engram-storage-gcs::connect`:
//! bounded 5 s connect (a blackholed endpoint surfaces to the caller's
//! retry layer instead of pinning an attempt for the OS default), no
//! operation timeout (blob bodies are GB-scale; per-attempt deadlines
//! live in `BlobClient`), and SDK retries DISABLED — one retry layer
//! per path: `BlobClient` owns blob retries, the host-operator's
//! reconcile re-asserts scaling, and secret resolution fails the
//! request to its caller. ALPN is left to the connector: AWS service
//! endpoints (S3 included) negotiate HTTP/1.1, so there is no h2
//! bulk-transfer cliff to pin against here; Phase G's blobbench run
//! against real S3 is the check on that assumption.
//!
//! # Credentials
//!
//! The SDK default chain, resolved lazily per request: env vars → IRSA
//! web-identity (the EKS path — file/env based, works in hostNetwork
//! pods, unlike GKE Workload Identity's metadata intercept) → shared
//! profile (static keys) → ECS → IMDSv2 (the node instance role).
//! `sso` and `credentials-process` are deliberately compiled out;
//! server-side identities never use them.

use std::time::Duration;

use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_smithy_http_client::{tls, Builder as HttpClientBuilder};
use aws_smithy_types::retry::RetryConfig;
use aws_smithy_types::timeout::TimeoutConfig;

/// Per-caller overrides applied on top of the SDK default chains.
/// `None` fields defer to the environment (`AWS_REGION`, profile, and
/// for the endpoint the service's production URL).
#[derive(Debug, Default, Clone)]
pub struct AwsOverrides {
    /// Region override. Blob callers thread `ENGRAM_S3_REGION` through
    /// here; `None` resolves via the SDK chain (env → profile → IMDS).
    pub region: Option<String>,
    /// Endpoint override for emulators and S3-compatible stores
    /// (MinIO, R2). Service-specific: only the constructing crate
    /// knows whether its service is being pointed at an emulator, so
    /// this rides the shared config rather than a per-service builder.
    pub endpoint_url: Option<String>,
}

/// Load the workspace-standard [`SdkConfig`].
///
/// The only supported way to obtain one. Installs the process
/// CryptoProvider, builds the ring-backed HTTP client, and applies the
/// transport posture documented on the crate. Service clients are then
/// `aws_sdk_<svc>::Client::new(&config)` in the consuming crate.
pub async fn sdk_config(overrides: AwsOverrides) -> SdkConfig {
    // `aws-config`'s own credential/region providers build HTTP
    // clients internally; the process provider must exist before any
    // of them do (same ordering rule as the GCS `connect`).
    engram_tls::install_provider();

    let http_client = HttpClientBuilder::new()
        .tls_provider(tls::Provider::Rustls(
            tls::rustls_provider::CryptoMode::Ring,
        ))
        .build_https();

    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .http_client(http_client)
        .timeout_config(
            TimeoutConfig::builder()
                .connect_timeout(Duration::from_secs(5))
                .build(),
        )
        .retry_config(RetryConfig::disabled());

    if let Some(region) = overrides.region {
        loader = loader.region(Region::new(region));
    }
    if let Some(endpoint) = overrides.endpoint_url {
        tracing::info!(endpoint = %endpoint, "aws sdk: endpoint override");
        loader = loader.endpoint_url(endpoint);
    }

    loader.load().await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract this crate exists for: constructing the config —
    /// which eagerly builds the ring-backed TLS client — must not
    /// panic in a process where nothing installed a provider first.
    /// The regression pin for the aws-lc/ring seam, mirroring
    /// `engram-tls::client_builds_without_a_preinstalled_provider`.
    ///
    /// Region + endpoint are pinned so `load()` resolves everything
    /// locally: no IMDS region probe, no network. Credential
    /// resolution is lazy (per request), so none happens here.
    #[tokio::test(flavor = "current_thread")]
    async fn sdk_config_builds_without_a_preinstalled_provider() {
        let cfg = sdk_config(AwsOverrides {
            region: Some("us-east-1".into()),
            endpoint_url: Some("http://127.0.0.1:1".into()),
        })
        .await;
        assert_eq!(cfg.region().map(|r| r.as_ref()), Some("us-east-1"));
        assert_eq!(cfg.endpoint_url(), Some("http://127.0.0.1:1"));
    }

    /// Overrides are optional: the default-chain path must also build
    /// (region may legitimately resolve to None off-cloud with no env
    /// set — that is the caller's concern, not a construction error).
    #[tokio::test(flavor = "current_thread")]
    async fn sdk_config_builds_with_no_overrides() {
        let _ = sdk_config(AwsOverrides::default()).await;
    }
}
