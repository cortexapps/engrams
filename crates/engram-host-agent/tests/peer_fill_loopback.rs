//! ADR 0095: the peer-chunk tier, end to end over real gRPC on loopback:
//! one serving host-agent gRPC server and the requester's `PeerFetcher`,
//! alone or as the fill engine's peer source over an empty store (so
//! every landed chunk came from the peer).
//!
//! Sized to the property (AGENTS.md): tiny chunks, no sleeps beyond
//! bounded polls.
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig, NO_CEILING};
use engram_chunk_store::manifest::ChunkHash;
use engram_core::error::SandboxError;
use engram_core::traits::sandbox::{HarnessDial, HarnessSink};
use engram_core::traits::{HostClient, SessionFence};
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::shell::ShellTunnel;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxId, SessionId};
use engram_fill::PeerSource;
use engram_host_agent::image_prefetch::ImageReadiness;
use engram_host_agent::peer_fill::{PeerFetcher, PeerHealth, PeerServe};
use engram_protocol::grpc_client::PeerChunkScope;
use engram_protocol::heartbeat::ManifestDigest;

/// The peer serve arm never touches `HostClient` — every method is a
/// loud `unreachable!()` so the test fails if that changes.
struct NoHost;

#[async_trait]
impl HostClient for NoHost {
    async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
        unreachable!("peer tier must not touch HostClient")
    }
    async fn destroy(&self, _: SandboxId, _: SessionFence) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        unreachable!()
    }
    async fn probe_sandbox(
        &self,
        _: SandboxId,
    ) -> Result<engram_core::types::sandbox::SandboxProbe, SandboxError> {
        unreachable!()
    }
    async fn exec_stream(&self, _: SandboxId, _: ExecRequest) -> Result<ExecStream, SandboxError> {
        unreachable!()
    }
    async fn snapshot(
        &self,
        _: SandboxId,
        _: SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        unreachable!()
    }
    async fn restore(
        &self,
        _: SnapshotMetadata,
        _: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        unreachable!()
    }
    async fn start_agent(
        &self,
        _: SandboxId,
        _: AgentSpec,
        _: SessionEgressPolicy,
        _: SessionFence,
    ) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn guest_ip(&self, _: SandboxId) -> Option<std::net::Ipv4Addr> {
        None
    }
    async fn bind_session(
        &self,
        _: SessionId,
        _: SandboxId,
        _: u64,
    ) -> Result<(), engram_core::SandboxError> {
        Ok(())
    }
    async fn unbind_session(&self, _: SessionId) {}
    async fn send_prompt(
        &self,
        _: SandboxId,
        _: String,
        _: String,
        _: Option<String>,
    ) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn pause(&self, _: SandboxId, _: SessionFence) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn resume(&self, _: SandboxId, _: SessionFence) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn proxy_shell(&self, _: SandboxId) -> Result<ShellTunnel, SandboxError> {
        unreachable!()
    }
    fn harness_dial(&self) -> HarnessDial {
        HarnessDial::Vsock
    }
    fn set_harness_sink(&self, _: HarnessSink) {}
}

fn mk_cache(dir: &tempfile::TempDir) -> ChunkCache {
    // Pin the free-space floor OFF: on a near-full dev/CI disk the
    // default 20%-free floor would evict just-seeded chunks on the
    // very next sweep (the same flake the in-crate `new_with_floor`
    // helper exists for). Safe: nextest runs each test in its own
    // process, so the env var can't race a sibling test.
    std::env::set_var(engram_chunk_store::cache::FREE_FLOOR_PCT_ENV_VAR, "0");
    ChunkCache::new(ChunkCacheConfig {
        root: dir.path().to_path_buf(),
        budget_bytes: NO_CEILING,
        sweep_debounce_ms: 0,
        eviction_enabled: true,
    })
}

fn pick_local_addr() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    addr
}

async fn boot_peer_server(peer: Arc<PeerServe>) -> String {
    let addr = pick_local_addr();
    let host_dyn: Arc<dyn HostClient> = Arc::new(NoHost);
    tokio::spawn(async move {
        let _ = engram_host_agent::grpc_server::boot(
            addr,
            host_dyn,
            engram_host_agent::session_epochs::ephemeral(),
            Some(peer),
        )
        .await;
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return format!("http://{addr}");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "peer gRPC server did not bind {addr} within 5s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Seed a serving cache with verified (servable) chunks. Uses the
/// verifying `put` — same terminal state a GCS populate leaves.
async fn seed_verified(cache: &ChunkCache, bodies: &[Vec<u8>]) -> Vec<ChunkHash> {
    let mut hashes = Vec::new();
    for b in bodies {
        let h = ChunkHash::of(b);
        cache
            .put(h, bytes::Bytes::copy_from_slice(b))
            .await
            .expect("seed put");
        hashes.push(h);
    }
    hashes
}

fn ready_digest(digest: &str) -> Arc<ImageReadiness> {
    let ready = ImageReadiness::new();
    ready.mark_ready(ManifestDigest::new(digest.to_string()));
    ready
}

const DIGEST: &str = "sha256:feedc0de";

/// A fill engine over `cache` and an empty store, with `peer` as its peer
/// source: a chunk the peer does not answer fails the fill.
fn engine_with_peer(
    cache: &ChunkCache,
    blob_dir: &tempfile::TempDir,
    peer: Arc<dyn PeerSource>,
) -> engram_fill::FillEngine {
    let store = engram_chunk_store::ChunkStore::new(Arc::new(
        engram_storage_local::LocalBlobStorage::new(blob_dir.path()),
    ))
    .with_chunk_cache(cache.clone());
    engram_fill::FillEngine::new(store, engram_fill::Config::default()).with_peer(peer)
}

/// The engine fills from the peer: a chunk of several frames and small
/// chunks land in the cache and serve onward at once.
#[tokio::test]
async fn an_engine_fill_lands_peer_chunks_that_serve_onward() {
    let serve_dir = tempfile::tempdir().unwrap();
    let land_dir = tempfile::tempdir().unwrap();
    let blob_dir = tempfile::tempdir().unwrap();
    let serve_cache = mk_cache(&serve_dir);
    let land_cache = mk_cache(&land_dir);
    // 5 MiB is more than the 4 MiB frame.
    let bodies: Vec<Vec<u8>> = vec![
        vec![0xAB; 5 * 1024 * 1024],
        b"tiny chunk one".to_vec(),
        vec![0x5C; 512 * 1024],
    ];
    let hashes = seed_verified(&serve_cache, &bodies).await;
    let peer = PeerServe::with_limits(serve_cache, ready_digest(DIGEST), 4, 4 * 1024 * 1024);
    let addr = boot_peer_server(peer).await;

    let health = PeerHealth::new();
    let fill = engine_with_peer(
        &land_cache,
        &blob_dir,
        PeerFetcher::spawn(
            addr.clone(),
            PeerChunkScope::BaseImage(DIGEST.into()),
            health.clone(),
        ),
    );
    let stats = fill
        .fill(hashes.clone(), engram_fill::Class::Background)
        .await
        .expect("every chunk from the peer");
    assert_eq!(stats.fetched, bodies.len());
    assert!(!health.is_lost(&addr));
    for (h, body) in hashes.iter().zip(&bodies) {
        let bytes = land_cache.read_resident(*h).await.unwrap();
        assert_eq!(bytes.as_deref(), Some(body.as_slice()), "chunk {h}");
    }
}

/// A peer that does not hold the image ready refuses the scope: the
/// fetch answers None and the peer is lost for the window.
#[tokio::test]
async fn a_stale_base_image_scope_loses_the_peer() {
    let serve_dir = tempfile::tempdir().unwrap();
    let serve_cache = mk_cache(&serve_dir);
    let hashes = seed_verified(&serve_cache, &[b"bytes".to_vec()]).await;
    let peer = PeerServe::with_limits(
        serve_cache,
        ready_digest("sha256:something-else"),
        4,
        4 * 1024 * 1024,
    );
    let addr = boot_peer_server(peer).await;

    let health = PeerHealth::new();
    let fetcher = PeerFetcher::spawn(
        addr.clone(),
        PeerChunkScope::BaseImage(DIGEST.into()),
        health.clone(),
    );
    assert!(fetcher.fetch(hashes[0]).await.is_none());
    assert!(health.is_lost(&addr));
}

/// A saturated peer answers RESOURCE_EXHAUSTED: the fetch answers None
/// and the peer stays healthy.
#[tokio::test]
async fn saturation_is_backpressure_not_death() {
    let serve_dir = tempfile::tempdir().unwrap();
    let serve_cache = mk_cache(&serve_dir);
    let hashes = seed_verified(&serve_cache, &[b"bytes".to_vec()]).await;
    // Zero serve streams: every open answers RESOURCE_EXHAUSTED.
    let peer = PeerServe::with_limits(serve_cache, ready_digest(DIGEST), 0, 4 * 1024 * 1024);
    let addr = boot_peer_server(peer).await;

    let health = PeerHealth::new();
    let fetcher = PeerFetcher::spawn(
        addr.clone(),
        PeerChunkScope::BaseImage(DIGEST.into()),
        health.clone(),
    );
    assert!(fetcher.fetch(hashes[0]).await.is_none());
    assert!(
        !health.is_lost(&addr),
        "backpressure must NOT mark the peer lost"
    );
}

/// Many concurrent requests share batched streams and each gets its own
/// chunk; a chunk the peer lacks answers None and does not mark it lost.
#[tokio::test]
async fn peer_fetcher_answers_concurrent_requests_and_misses() {
    let serve_dir = tempfile::tempdir().unwrap();
    let serve_cache = mk_cache(&serve_dir);
    let bodies: Vec<Vec<u8>> = (0..100u8).map(|i| vec![i; 4096 + i as usize]).collect();
    let hashes = seed_verified(&serve_cache, &bodies).await;
    let peer = PeerServe::with_limits(serve_cache, ready_digest(DIGEST), 4, 4 * 1024 * 1024);
    let addr = boot_peer_server(peer).await;

    let health = PeerHealth::new();
    let fetcher = PeerFetcher::spawn(
        addr.clone(),
        PeerChunkScope::BaseImage(DIGEST.into()),
        health.clone(),
    );
    let answers = futures::future::join_all(hashes.iter().map(|h| fetcher.fetch(*h))).await;
    for (answer, body) in answers.iter().zip(&bodies) {
        assert_eq!(answer.as_deref(), Some(body.as_slice()));
    }

    assert!(fetcher
        .fetch(ChunkHash::of(b"never seeded"))
        .await
        .is_none());
    assert!(!health.is_lost(&addr), "a miss must not mark the peer lost");
}

/// A dead peer is marked lost on the first failed dial; later requests
/// answer None at once instead of dialing again.
#[tokio::test]
async fn peer_fetcher_on_a_dead_peer_answers_none_and_stays_fast() {
    let addr = format!("http://{}", pick_local_addr()); // nothing listening
    let health = PeerHealth::new();
    let fetcher = PeerFetcher::spawn(
        addr.clone(),
        PeerChunkScope::Snapshot(engram_core::SnapshotId::new()),
        health.clone(),
    );
    assert!(fetcher.fetch(ChunkHash::of(b"a")).await.is_none());
    assert!(health.is_lost(&addr));
    let started = std::time::Instant::now();
    assert!(fetcher.fetch(ChunkHash::of(b"b")).await.is_none());
    assert!(started.elapsed() < Duration::from_millis(100));
}
