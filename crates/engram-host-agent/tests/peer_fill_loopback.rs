//! ADR 0095: the standing peer-chunk tier, end to end over real gRPC
//! on loopback — two chunk caches, one serving host-agent gRPC server,
//! the real `PeerFillClient` pull machinery.
//!
//! Pins the tier's whole contract without a VM or a blob store on the
//! REQUESTER side (structurally zero GCS involvement for peer-landed
//! chunks — there is nothing to fall back to, so every assertion about
//! landed bytes is an assertion about the peer path):
//!   1. batch pull lands CRC-checked bytes as unverified-origin; the
//!      scrubber verifies them; the serve-onward gate opens only then;
//!   2. a locally-missing hash streams a `missing` marker (peer stays
//!      healthy);
//!   3. a stale BaseImage scope is rejected (`scope_reject`), which the
//!      requester treats as a failed window;
//!   4. serve-semaphore saturation is RESOURCE_EXHAUSTED = backpressure,
//!      never peer death;
//!   5. a dead peer costs one bounded dial and is health-cached lost.
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
use engram_host_agent::image_prefetch::ImageReadiness;
use engram_host_agent::peer_fill::{pull_chunks_from_peer, PeerHealth, PeerServe};
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
    async fn apply_egress_policy(&self, _: SessionEgressPolicy) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn guest_ip(&self, _: SandboxId) -> Option<std::net::Ipv4Addr> {
        None
    }
    async fn bind_session(&self, _: SessionId, _: SandboxId, _: u64) {}
    async fn unbind_session(&self, _: SessionId) {}
    async fn send_prompt(&self, _: SandboxId, _: String, _: String) -> Result<(), SandboxError> {
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
            None,
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
        cache.put(h, b).await.expect("seed put");
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

#[tokio::test]
async fn pull_lands_scrubs_and_opens_the_serve_gate() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("engram_host_agent=debug")
        .try_init();
    let serve_dir = tempfile::tempdir().unwrap();
    let land_dir = tempfile::tempdir().unwrap();
    let serve_cache = mk_cache(&serve_dir);
    let land_cache = mk_cache(&land_dir);

    // Mixed sizes: several frames for one chunk (5 MiB > the 4 MiB
    // frame), tiny single-frame chunks for the batch shape.
    let bodies: Vec<Vec<u8>> = vec![
        vec![0xAB; 5 * 1024 * 1024],
        b"tiny chunk one".to_vec(),
        vec![0x5C; 512 * 1024],
        b"tiny chunk two".to_vec(),
    ];
    let hashes = seed_verified(&serve_cache, &bodies).await;

    let peer = PeerServe::with_limits(serve_cache, ready_digest(DIGEST), 4, 4 * 1024 * 1024);
    let addr = boot_peer_server(peer).await;

    let health = PeerHealth::new();
    let stats = pull_chunks_from_peer(
        &addr,
        PeerChunkScope::BaseImage(DIGEST.into()),
        &hashes,
        &land_cache,
        &health,
    )
    .await;
    assert!(
        !stats.failed && !stats.backpressure,
        "clean pull: {stats:?}"
    );
    assert_eq!(stats.landed, bodies.len());
    assert_eq!(stats.missing, 0);
    assert_eq!(
        stats.landed_bytes,
        bodies.iter().map(|b| b.len() as u64).sum::<u64>()
    );
    assert!(!health.is_lost(&addr));

    for (h, body) in hashes.iter().zip(&bodies) {
        // Landed + locally readable...
        assert!(land_cache.contains(*h).await, "chunk {h} not resident");
        // ...but unverified-origin: NOT servable onward pre-scrub (the
        // one-hop containment property).
        assert!(
            land_cache
                .read_verified_for_serve(*h)
                .await
                .unwrap()
                .is_none(),
            "peer-landed chunk {h} served onward before scrub"
        );
        let _ = body;
    }

    // Scrubber drains the backlog; every chunk becomes servable and
    // byte-identical to the source.
    let _scrubber = land_cache.spawn_scrubber(u64::MAX);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    'outer: loop {
        for (h, body) in hashes.iter().zip(&bodies) {
            match land_cache.read_verified_for_serve(*h).await.unwrap() {
                Some(bytes) if bytes.as_ref() == body.as_slice() => {}
                Some(_) => panic!("scrubbed chunk {h} has wrong bytes"),
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "scrub did not drain within 5s"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue 'outer;
                }
            }
        }
        break;
    }
}

#[tokio::test]
async fn missing_hash_is_an_honest_miss_not_a_fault() {
    let serve_dir = tempfile::tempdir().unwrap();
    let land_dir = tempfile::tempdir().unwrap();
    let serve_cache = mk_cache(&serve_dir);
    let land_cache = mk_cache(&land_dir);
    let hashes = seed_verified(&serve_cache, &[b"present".to_vec()]).await;

    let peer = PeerServe::with_limits(serve_cache, ready_digest(DIGEST), 4, 4 * 1024 * 1024);
    let addr = boot_peer_server(peer).await;

    let absent = ChunkHash::of(b"never seeded");
    let want = vec![hashes[0], absent];
    let health = PeerHealth::new();
    let stats = pull_chunks_from_peer(
        &addr,
        PeerChunkScope::BaseImage(DIGEST.into()),
        &want,
        &land_cache,
        &health,
    )
    .await;
    assert!(!stats.failed, "a miss must not fail the window");
    assert_eq!(stats.landed, 1);
    assert_eq!(stats.missing, 1);
    assert!(!health.is_lost(&addr), "a miss must not mark the peer lost");
    assert!(land_cache.contains(hashes[0]).await);
    assert!(!land_cache.contains(absent).await);
}

#[tokio::test]
async fn stale_base_image_scope_is_rejected() {
    let serve_dir = tempfile::tempdir().unwrap();
    let land_dir = tempfile::tempdir().unwrap();
    let serve_cache = mk_cache(&serve_dir);
    let land_cache = mk_cache(&land_dir);
    let hashes = seed_verified(&serve_cache, &[b"bytes".to_vec()]).await;

    // Ready set does NOT contain the requested digest.
    let peer = PeerServe::with_limits(
        serve_cache,
        ready_digest("sha256:something-else"),
        4,
        4 * 1024 * 1024,
    );
    let addr = boot_peer_server(peer).await;

    let health = PeerHealth::new();
    let stats = pull_chunks_from_peer(
        &addr,
        PeerChunkScope::BaseImage(DIGEST.into()),
        &hashes,
        &land_cache,
        &health,
    )
    .await;
    assert!(stats.failed, "stale scope must fail the window");
    assert_eq!(stats.landed, 0);
    assert!(!land_cache.contains(hashes[0]).await);
}

#[tokio::test]
async fn saturation_is_backpressure_not_death() {
    let serve_dir = tempfile::tempdir().unwrap();
    let land_dir = tempfile::tempdir().unwrap();
    let serve_cache = mk_cache(&serve_dir);
    let land_cache = mk_cache(&land_dir);
    let hashes = seed_verified(&serve_cache, &[b"bytes".to_vec()]).await;

    // Zero serve streams: every open answers RESOURCE_EXHAUSTED.
    let peer = PeerServe::with_limits(serve_cache, ready_digest(DIGEST), 0, 4 * 1024 * 1024);
    let addr = boot_peer_server(peer).await;

    let health = PeerHealth::new();
    let stats = pull_chunks_from_peer(
        &addr,
        PeerChunkScope::BaseImage(DIGEST.into()),
        &hashes,
        &land_cache,
        &health,
    )
    .await;
    assert!(
        stats.backpressure,
        "saturation must surface as backpressure"
    );
    assert!(!stats.failed, "backpressure is not a peer fault");
    assert!(
        !health.is_lost(&addr),
        "backpressure must NOT mark the peer lost"
    );
}

#[tokio::test]
async fn dead_peer_costs_one_bounded_dial_and_is_cached_lost() {
    let land_dir = tempfile::tempdir().unwrap();
    let land_cache = mk_cache(&land_dir);
    let addr = format!("http://{}", pick_local_addr()); // nothing listening
    let hashes = vec![ChunkHash::of(b"whatever")];

    let health = PeerHealth::new();
    let started = std::time::Instant::now();
    let stats = pull_chunks_from_peer(
        &addr,
        PeerChunkScope::Snapshot(engram_core::SnapshotId::new()),
        &hashes,
        &land_cache,
        &health,
    )
    .await;
    assert!(stats.failed);
    assert_eq!(stats.landed, 0);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "dead-peer window must cost ~one 2s dial, took {:?}",
        started.elapsed()
    );
    assert!(health.is_lost(&addr), "dead peer must be health-cached");

    // Second window inside the lost-cache: no dial at all — returns
    // failed immediately.
    let started = std::time::Instant::now();
    let stats = pull_chunks_from_peer(
        &addr,
        PeerChunkScope::Snapshot(engram_core::SnapshotId::new()),
        &hashes,
        &land_cache,
        &health,
    )
    .await;
    assert!(stats.failed);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "health-cached peer must be skipped without dialing"
    );
}
