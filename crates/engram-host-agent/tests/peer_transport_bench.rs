//! ADR 0095 transport bench — MANUAL (`#[ignore]`), not a CI test.
//!
//! Measures the peer-chunk pull's end-to-end loopback throughput (real
//! gRPC serve arm + real pull machinery + real CRC + real cache
//! landing) against a raw-TCP baseline moving the same bytes, to gate
//! the transport shape: tuned tonic ships if it sustains ≥ the local-
//! NVMe write rate (~700 MB/s); raw TCP is only worth its extra
//! surface if H2 overhead provably caps below that.
//!
//! Run (numbers belong in the ADR; run on Linux for the real gate —
//! macOS loopback still exposes protocol overhead honestly):
//!
//! ```sh
//! cargo nextest run -p engram-host-agent --test peer_transport_bench \
//!   --run-ignored all --no-capture
//! ```
//!
//! Knobs: ENGRAM_PEER_PULL_CONNS / ENGRAM_PEER_FRAME_BYTES /
//! ENGRAM_PEER_SERVE_STREAMS (see peer_fill.rs), and
//! BENCH_TOTAL_MIB (default 512).
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig, NO_CEILING};
use engram_chunk_store::manifest::ChunkHash;
use engram_host_agent::image_prefetch::ImageReadiness;
use engram_host_agent::peer_fill::{pull_chunks_from_peer, PeerHealth, PeerServe};
use engram_protocol::grpc_client::PeerChunkScope;
use engram_protocol::heartbeat::ManifestDigest;

mod peer_bench_support {
    use async_trait::async_trait;
    use engram_core::error::SandboxError;
    use engram_core::traits::sandbox::{HarnessDial, HarnessSink};
    use engram_core::traits::{HostClient, SessionFence};
    use engram_core::types::egress::SessionEgressPolicy;
    use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
    use engram_core::types::shell::ShellTunnel;
    use engram_core::types::snapshot::SnapshotMetadata;
    use engram_core::{SandboxId, SessionId};

    pub struct NoHost;

    #[async_trait]
    impl HostClient for NoHost {
        async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
            unreachable!()
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
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
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
        async fn bind_session(&self, _: SessionId, _: SandboxId, _: u64) {}
        async fn unbind_session(&self, _: SessionId) {}
        async fn send_prompt(
            &self,
            _: SandboxId,
            _: String,
            _: String,
            _mode: Option<String>,
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
}

fn mk_cache(dir: &tempfile::TempDir) -> ChunkCache {
    std::env::set_var(engram_chunk_store::cache::FREE_FLOOR_PCT_ENV_VAR, "0");
    ChunkCache::new(ChunkCacheConfig {
        root: dir.path().to_path_buf(),
        budget_bytes: NO_CEILING,
        sweep_debounce_ms: 60_000,
        eviction_enabled: true,
    })
}

fn total_mib() -> usize {
    std::env::var("BENCH_TOTAL_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512)
}

const DIGEST: &str = "sha256:bench";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual transport bench — see module docs"]
async fn bench_tuned_grpc_pull() {
    let serve_dir = tempfile::tempdir().unwrap();
    let land_dir = tempfile::tempdir().unwrap();
    let serve_cache = mk_cache(&serve_dir);
    let land_cache = mk_cache(&land_dir);

    // 16 MiB disk-shaped chunks + one tail of 512 KiB memory-shaped
    // chunks, matching the prod mix.
    let disk_chunks = total_mib() / 16;
    let mem_chunks = 256; // 128 MiB of 512 KiB objects
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    for i in 0..disk_chunks {
        let mut b = vec![0u8; 16 * 1024 * 1024];
        b[0] = i as u8; // distinct hashes
        b[1] = 0xD1;
        bodies.push(b);
    }
    for i in 0..mem_chunks {
        let mut b = vec![0u8; 512 * 1024];
        b[0] = i as u8;
        b[1] = (i >> 8) as u8;
        b[2] = 0x3E;
        bodies.push(b);
    }
    let mut hashes = Vec::new();
    for b in &bodies {
        let h = ChunkHash::of(b);
        serve_cache.put(h, b).await.unwrap();
        hashes.push(h);
    }
    let total_bytes: usize = bodies.iter().map(Vec::len).sum();
    drop(bodies);

    let ready = ImageReadiness::new();
    ready.mark_ready(ManifestDigest::new(DIGEST.to_string()));
    let peer = PeerServe::new(serve_cache, ready);
    let addr = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = listener.local_addr().unwrap();
        drop(listener);
        let host: Arc<dyn engram_core::traits::HostClient> = Arc::new(peer_bench_support::NoHost);
        tokio::spawn(engram_host_agent::grpc_server::boot(
            a,
            host,
            None,
            engram_host_agent::session_epochs::ephemeral(),
            Some(peer),
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;
        format!("http://{a}")
    };

    let health = PeerHealth::new();
    let started = Instant::now();
    let stats = pull_chunks_from_peer(
        &addr,
        PeerChunkScope::BaseImage(DIGEST.into()),
        &hashes,
        &land_cache,
        &health,
    )
    .await;
    let elapsed = started.elapsed();
    assert!(!stats.failed, "bench pull failed: {stats:?}");
    assert_eq!(stats.landed, hashes.len());
    let mbps = (total_bytes as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
    println!(
        "tuned gRPC pull: {} chunks / {:.0} MiB in {:.2?} = {:.0} MiB/s \
         (conns={}, frame={} B)",
        hashes.len(),
        total_bytes as f64 / (1024.0 * 1024.0),
        elapsed,
        mbps,
        std::env::var("ENGRAM_PEER_PULL_CONNS").unwrap_or_else(|_| "4 (default)".into()),
        std::env::var("ENGRAM_PEER_FRAME_BYTES").unwrap_or_else(|_| "4 MiB (default)".into()),
    );
}

/// Raw-TCP baseline: the theoretical ceiling for this box's loopback +
/// buffer handling, no protocol at all. The gap between this and the
/// tuned gRPC number is the H2+proto tax.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual transport bench — see module docs"]
async fn bench_raw_tcp_baseline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let total = total_mib() * 1024 * 1024;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let chunk = vec![0xABu8; 4 * 1024 * 1024];
        let mut sent = 0usize;
        while sent < total {
            sock.write_all(&chunk).await.unwrap();
            sent += chunk.len();
        }
    });
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut got = 0usize;
    let started = Instant::now();
    while got < total {
        let n = sock.read(&mut buf).await.unwrap();
        assert!(n > 0);
        got += n;
    }
    let elapsed = started.elapsed();
    println!(
        "raw TCP baseline: {:.0} MiB in {:.2?} = {:.0} MiB/s",
        total as f64 / (1024.0 * 1024.0),
        elapsed,
        (total as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64(),
    );
}
