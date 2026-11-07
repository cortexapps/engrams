//! ADR 0095: the standing peer-chunk tier — both halves.
//!
//! **Serve** ([`PeerServe`], driven by `grpc_server`'s `PeerChunkGet`
//! arm): stream cache-resident chunks to any fleet peer, batch
//! semantics, resident-only (a peer can never induce a GCS read here),
//! bounded by a host-global stream semaphore.
//!
//! **Pull** ([`pull_chunks_from_peer`] for bulk; [`PeerFetcher`] as the
//! fill engine's peer tier): dial a coordinator-hinted sibling, stream
//! the wanted set over a few fat connections, and reassemble each chunk
//! from frames that pass their CRC32C and arrive in order, contiguous
//! from offset 0 up to the `last` frame. Peer bytes are trusted, not
//! hashed: the peer hashed each chunk when it stored it. Bulk chunks
//! land through `ChunkCache::put_trusted`. The pull is a
//! best-effort PRE-PASS: callers re-check residency afterwards and
//! complete whatever is still missing through the existing verified
//! GCS path, so a dead/missing/saturated peer degrades to today's
//! behavior with at most the bounded dials of one window.
//!
//! Transport shape (the loopback bench in
//! `tests/peer_transport_bench.rs` gates these defaults): a few
//! separate TCP connections (HTTP/2 connection-level flow control and
//! GCP single-flow bandwidth both cap a single channel), large static
//! windows + adaptive flow control, 4 MiB frames. The teleport
//! transport's 8×1 MiB-frames-on-one-channel shape measured ~20 MB/s
//! per stream — a config artifact this module exists to not repeat.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_chunk_store::cache::{ChunkCache, CHUNK_FILL_BYTES_TOTAL, CHUNK_FILL_TOTAL};
use engram_chunk_store::manifest::ChunkHash;
use engram_protocol::grpc::{PeerChunkFrame, PeerChunkGetRequest};
use engram_protocol::grpc_client::{GrpcHostClient, PeerChunkScope};
use parking_lot::Mutex;
use tokio::sync::Semaphore;

use crate::image_prefetch::ImageReadiness;

/// Host-global cap on concurrent `PeerChunkGet` serve streams. Beyond
/// it the server answers `RESOURCE_EXHAUSTED` — backpressure the
/// requester treats as "this batch goes to GCS", never as peer death.
pub const SERVE_STREAMS_ENV: &str = "ENGRAM_PEER_SERVE_STREAMS";
const DEFAULT_SERVE_STREAMS: usize = 16;

/// Frame payload size for serve streams. 4 MiB: large enough that
/// per-frame overhead is noise, small enough that a frame + proto
/// envelope stays under the fleet-wide 32 MiB decode cap.
pub const FRAME_BYTES_ENV: &str = "ENGRAM_PEER_FRAME_BYTES";
const DEFAULT_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Parallel TCP connections per bulk pull. Each is its own HTTP/2
/// connection (window + GCP single-flow limits are per-connection).
pub const PULL_CONNS_ENV: &str = "ENGRAM_PEER_PULL_CONNS";
const DEFAULT_PULL_CONNS: usize = 4;

/// The one-dial degradation contract (ADR 0095): connect timeout per
/// hinted address, and how long a failed peer stays marked lost.
const DIAL_TIMEOUT: Duration = Duration::from_secs(2);
const PEER_LOST_WINDOW: Duration = Duration::from_secs(30);

/// HTTP/2 stream/connection windows for peer pulls. Tonic's defaults
/// (64 KiB / 1 MiB) cap a stream at ~20 MB/s; these put the
/// bandwidth-delay product comfortably above NVMe rate. Adaptive
/// flow control is also enabled and may grow past these.
const STREAM_WINDOW: u32 = 16 * 1024 * 1024;
const CONN_WINDOW: u32 = 32 * 1024 * 1024;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

pub fn serve_streams_from_env() -> usize {
    env_usize(SERVE_STREAMS_ENV, DEFAULT_SERVE_STREAMS)
}

pub fn frame_bytes_from_env() -> usize {
    env_usize(FRAME_BYTES_ENV, DEFAULT_FRAME_BYTES)
}

fn pull_conns_from_env() -> usize {
    env_usize(PULL_CONNS_ENV, DEFAULT_PULL_CONNS)
}

// ---------------------------------------------------------------------
// Serve side
// ---------------------------------------------------------------------

/// State behind the `PeerChunkGet` serve arm. Constructed in `lib.rs`
/// alongside the prefetch supervisor and handed to `grpc_server::boot`;
/// absent (None) on cache-less hosts (Process backend), which answer
/// `unavailable`.
pub struct PeerServe {
    pub cache: ChunkCache,
    /// The heartbeat's ready-images view — the `BaseImage` scope check.
    /// Ready (not merely prestaging): a host that hasn't finished
    /// warming an image may not hold its chunks, and the coordinator
    /// only hints ready hosts anyway.
    pub ready: Arc<ImageReadiness>,
    semaphore: Arc<Semaphore>,
    frame_bytes: usize,
}

impl PeerServe {
    pub fn new(cache: ChunkCache, ready: Arc<ImageReadiness>) -> Arc<Self> {
        Self::with_limits(
            cache,
            ready,
            serve_streams_from_env(),
            frame_bytes_from_env(),
        )
    }

    /// Explicit-limits constructor — tests pin the stream cap / frame
    /// size instead of racing process-global env vars.
    pub fn with_limits(
        cache: ChunkCache,
        ready: Arc<ImageReadiness>,
        streams: usize,
        frame_bytes: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            cache,
            ready,
            semaphore: Arc::new(Semaphore::new(streams)),
            frame_bytes: frame_bytes.max(1),
        })
    }

    /// Try to claim a serve-stream slot. `None` ⇒ saturated
    /// (`RESOURCE_EXHAUSTED` to the caller).
    pub fn try_claim_stream(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        match self.semaphore.clone().try_acquire_owned() {
            Ok(p) => Some(p),
            Err(_) => {
                metrics::counter!("engram_peer_serve_total", "outcome" => "saturated").increment(1);
                None
            }
        }
    }

    /// Produce the frame stream for one request. Runs as a spawned
    /// task feeding `tx`; holds `permit` for its lifetime. Chunks are
    /// served resident-only: an absent chunk is a terminal `missing`
    /// frame for that item (the requester sources it from GCS; never a
    /// fault).
    pub async fn stream_frames(
        self: Arc<Self>,
        hashes: Vec<ChunkHash>,
        tx: tokio::sync::mpsc::Sender<Result<PeerChunkFrame, tonic::Status>>,
        _permit: tokio::sync::OwnedSemaphorePermit,
    ) {
        for (idx, hash) in hashes.into_iter().enumerate() {
            let item_idx = idx as u32;
            let bytes = match self.cache.read_resident(hash).await {
                Ok(Some(b)) => b,
                Ok(None) => {
                    metrics::counter!("engram_peer_serve_total", "outcome" => "missing")
                        .increment(1);
                    let frame = PeerChunkFrame {
                        item_idx,
                        offset: 0,
                        data: Vec::new(),
                        last: true,
                        missing: true,
                        crc32c: 0,
                    };
                    if tx.send(Ok(frame)).await.is_err() {
                        return; // requester hung up
                    }
                    continue;
                }
                Err(e) => {
                    // Local read error — fail the stream loudly; the
                    // requester marks this peer lost for the window and
                    // completes via GCS.
                    let _ = tx
                        .send(Err(tonic::Status::internal(format!(
                            "peer serve: read {hash}: {e}"
                        ))))
                        .await;
                    return;
                }
            };
            metrics::counter!("engram_peer_serve_total", "outcome" => "hit").increment(1);
            metrics::counter!("engram_peer_serve_bytes_total").increment(bytes.len() as u64);
            let mut offset = 0usize;
            let total = bytes.len();
            loop {
                let end = (offset + self.frame_bytes).min(total);
                let data = bytes[offset..end].to_vec();
                let frame = PeerChunkFrame {
                    item_idx,
                    offset: offset as u64,
                    crc32c: crc32c::crc32c(&data),
                    last: end == total,
                    missing: false,
                    data,
                };
                if tx.send(Ok(frame)).await.is_err() {
                    return;
                }
                if end == total {
                    break;
                }
                offset = end;
            }
        }
    }
}

// ---------------------------------------------------------------------
// Pull side
// ---------------------------------------------------------------------

/// Requester-side health cache: a peer that failed a dial or died
/// mid-stream is "lost" for [`PEER_LOST_WINDOW`] — later pulls in the
/// window skip it without dialing (the one-dial contract).
/// Backpressure (`RESOURCE_EXHAUSTED`) and honest misses never land
/// here.
#[derive(Default)]
pub struct PeerHealth {
    lost_until: Mutex<HashMap<String, Instant>>,
}

impl PeerHealth {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn is_lost(&self, addr: &str) -> bool {
        match self.lost_until.lock().get(addr) {
            Some(until) => crate::time_source::metrics_now() < *until,
            None => false,
        }
    }

    pub fn mark_lost(&self, addr: &str) {
        self.lost_until.lock().insert(
            addr.to_string(),
            crate::time_source::metrics_now() + PEER_LOST_WINDOW,
        );
        metrics::counter!("engram_peer_health_trips_total").increment(1);
    }
}

/// Outcome of one bulk pull window — counts for logs/metrics. The
/// authoritative "what still needs GCS" signal is residency
/// (`cache.contains`), re-checked by the caller after the pull.
#[derive(Debug, Default, Clone, Copy)]
pub struct PullStats {
    pub landed: usize,
    pub landed_bytes: u64,
    pub missing: usize,
    /// Dial/stream failure (peer marked lost) — distinct from
    /// `backpressure` (peer healthy, batch deferred to GCS).
    pub failed: bool,
    pub backpressure: bool,
}

/// Tune a peer-pull endpoint for throughput: big static windows +
/// adaptive flow control + TCP nodelay off the table (we send MiBs).
fn peer_endpoint(addr: &str) -> Result<tonic::transport::Endpoint, String> {
    Ok(tonic::transport::Endpoint::from_shared(addr.to_string())
        .map_err(|e| format!("bad peer addr {addr}: {e}"))?
        .connect_timeout(DIAL_TIMEOUT)
        .initial_stream_window_size(Some(STREAM_WINDOW))
        .initial_connection_window_size(Some(CONN_WINDOW))
        .http2_adaptive_window(true)
        .tcp_nodelay(true))
}

/// Bulk-pull `hashes` from the peer at `addr` into `cache` through
/// [`ChunkCache::put_trusted`]. Best-effort pre-pass (see module
/// docs): every outcome short of success leaves the caller's GCS
/// completion pass to pick up the remainder.
///
/// Concurrency: `hashes` is sharded contiguously (hot-first order
/// preserved per shard) across [`PULL_CONNS_ENV`] separate TCP
/// connections, one request-stream each.
pub async fn pull_chunks_from_peer(
    addr: &str,
    scope: PeerChunkScope,
    hashes: &[ChunkHash],
    cache: &ChunkCache,
    health: &PeerHealth,
) -> PullStats {
    let mut stats = PullStats::default();
    if hashes.is_empty() {
        return stats;
    }
    if health.is_lost(addr) {
        metrics::counter!("engram_peer_dial_total", "outcome" => "lost_cached").increment(1);
        stats.failed = true;
        return stats;
    }
    let conns = pull_conns_from_env().min(hashes.len());
    let endpoint = match peer_endpoint(addr) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(addr, error = %e, "peer pull: bad endpoint");
            health.mark_lost(addr);
            stats.failed = true;
            return stats;
        }
    };
    // Dial the connections up front — ONE bounded dial attempt per
    // connection, and any failure marks the peer lost for the window
    // (the degradation contract). Connections dial concurrently, so
    // worst-case added latency is one DIAL_TIMEOUT, not conns × it.
    let dials = futures::future::join_all((0..conns).map(|_| endpoint.connect()));
    let mut clients = Vec::with_capacity(conns);
    for dial in dials.await {
        match dial {
            Ok(channel) => clients.push(GrpcHostClient::new(channel)),
            Err(e) => {
                metrics::counter!("engram_peer_dial_total", "outcome" => "timeout").increment(1);
                tracing::warn!(addr, error = %e, "peer pull: dial failed — peer lost for window");
                health.mark_lost(addr);
                stats.failed = true;
                return stats;
            }
        }
    }
    metrics::counter!("engram_peer_dial_total", "outcome" => "ok").increment(1);

    let shard_len = hashes.len().div_ceil(clients.len()).max(1);
    let mut tasks = Vec::with_capacity(clients.len());
    for (client, shard) in clients.into_iter().zip(hashes.chunks(shard_len)) {
        let shard: Vec<ChunkHash> = shard.to_vec();
        let scope = scope.clone();
        let cache = cache.clone();
        tasks.push(tokio::spawn(async move {
            pull_shard(client, scope, shard, cache).await
        }));
    }
    let mut any_stream_failure = false;
    let mut any_backpressure = false;
    for t in tasks {
        match t.await {
            Ok(shard_stats) => {
                stats.landed += shard_stats.landed;
                stats.landed_bytes += shard_stats.landed_bytes;
                stats.missing += shard_stats.missing;
                any_stream_failure |= shard_stats.failed;
                any_backpressure |= shard_stats.backpressure;
            }
            Err(e) => {
                tracing::warn!(addr, error = %e, "peer pull: shard task join failed");
                any_stream_failure = true;
            }
        }
    }
    if any_stream_failure {
        health.mark_lost(addr);
        stats.failed = true;
    }
    stats.backpressure = any_backpressure;
    stats
}

/// One connection's shard: a single `PeerChunkGet` request, frames
/// reassembled per item by [`Reassembly`], landed through
/// `ChunkCache::put_trusted`.
async fn pull_shard(
    client: GrpcHostClient,
    scope: PeerChunkScope,
    hashes: Vec<ChunkHash>,
    cache: ChunkCache,
) -> PullStats {
    use futures::StreamExt;
    let mut stats = PullStats::default();
    let wire_hashes: Vec<[u8; 32]> = hashes.iter().map(|h| *h.as_bytes()).collect();
    let mut stream = match client.peer_chunk_get(wire_hashes, scope).await {
        Ok(s) => s,
        Err(e) => {
            // RESOURCE_EXHAUSTED (→ LimitExceeded) is backpressure, not
            // death: the serve semaphore is full; this batch goes to
            // GCS and the peer stays healthy.
            if matches!(&e, engram_core::SandboxError::LimitExceeded(_)) {
                metrics::counter!("engram_peer_pull_total", "outcome" => "backpressure")
                    .increment(1);
                stats.backpressure = true;
            } else {
                tracing::warn!(error = %e, "peer pull: stream open failed");
                stats.failed = true;
            }
            return stats;
        }
    };
    let mut reassembly = Reassembly::new(hashes.len());
    while let Some(frame) = stream.next().await {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "peer pull: mid-stream error");
                stats.failed = true;
                return stats;
            }
        };
        let (idx, body) = match reassembly.push(frame) {
            Ok(Assembled::Partial) => continue,
            Ok(Assembled::Missing(_)) => {
                stats.missing += 1;
                continue;
            }
            Ok(Assembled::Whole(idx, body)) => (idx, body),
            Err(e) => {
                tracing::warn!(error = %e, "peer pull: bad frame");
                stats.failed = true;
                return stats;
            }
        };
        let hash = hashes[idx];
        let len = body.len();
        // A restore waits for these chunks.
        if let Err(e) = cache
            .put_trusted(hash, bytes::Bytes::from(body), engram_fill::Class::Prefix)
            .await
        {
            // Local landing failure (ENOSPC…) — not the peer's
            // fault; skip this chunk (GCS pass covers it) and keep
            // draining the stream.
            tracing::warn!(hash = %hash, error = %e, "peer pull: landing failed");
        } else {
            stats.landed += 1;
            stats.landed_bytes += len as u64;
            metrics::counter!(CHUNK_FILL_TOTAL, "source" => "peer").increment(1);
            metrics::counter!(CHUNK_FILL_BYTES_TOTAL, "source" => "peer").increment(len as u64);
        }
    }
    stats
}

/// What one frame completes in a [`Reassembly`].
#[derive(Debug, PartialEq)]
enum Assembled {
    /// The frame extends an open item.
    Partial,
    /// The peer does not hold the item at this index.
    Missing(usize),
    /// The item at this index is whole.
    Whole(usize, Vec<u8>),
}

/// Reassembles one `PeerChunkGet` stream. The serve side sends one item
/// at a time, each as frames contiguous from offset 0 that end with a
/// `last` frame. Any other frame is a failed peer answer: a frame that
/// fails its CRC32C, names an unknown item, starts a new item before the
/// open one ends, or does not start where the bytes so far end.
struct Reassembly {
    items: usize,
    open: Option<u32>,
    bytes: Vec<u8>,
}

impl Reassembly {
    fn new(items: usize) -> Self {
        Self {
            items,
            open: None,
            bytes: Vec::new(),
        }
    }

    fn push(&mut self, frame: PeerChunkFrame) -> Result<Assembled, String> {
        let idx = frame.item_idx;
        if (idx as usize) >= self.items {
            return Err(format!("unknown item index {idx}"));
        }
        if let Some(open) = self.open.filter(|&open| open != idx || frame.missing) {
            return Err(format!(
                "item {open} is open when a frame for item {idx} arrives"
            ));
        }
        if frame.missing {
            return Ok(Assembled::Missing(idx as usize));
        }
        if crc32c::crc32c(&frame.data) != frame.crc32c {
            return Err(format!("item {idx}: frame CRC32C mismatch"));
        }
        if frame.offset != self.bytes.len() as u64 {
            return Err(format!(
                "item {idx}: frame at offset {} after {} bytes",
                frame.offset,
                self.bytes.len()
            ));
        }
        self.bytes.extend_from_slice(&frame.data);
        if frame.last {
            self.open = None;
            Ok(Assembled::Whole(
                idx as usize,
                std::mem::take(&mut self.bytes),
            ))
        } else {
            self.open = Some(idx);
            Ok(Assembled::Partial)
        }
    }
}

/// Parse + validate a `PeerChunkGetRequest`'s hashes (32-byte each).
pub fn parse_request_hashes(req: &PeerChunkGetRequest) -> Result<Vec<ChunkHash>, String> {
    req.hashes
        .iter()
        .map(|raw| {
            <[u8; 32]>::try_from(raw.as_slice())
                .map(ChunkHash::from_bytes)
                .map_err(|_| format!("bad chunk hash length {} (want 32)", raw.len()))
        })
        .collect()
}

/// Chunks asked of one [`PeerFetcher`] stream together, at most.
const PEER_BATCH: usize = 64;
/// [`PeerFetcher`] streams in flight to one peer.
const PEER_BATCHES_IN_FLIGHT: usize = 8;

type PeerWaiter = tokio::sync::oneshot::Sender<Option<bytes::Bytes>>;

/// A hinted peer as a fill-engine source. Requests that arrive together go
/// out as one `PeerChunkGet` stream, several streams at once, and each
/// chunk goes back to its waiter as its last frame lands. A lost peer
/// answers `None` at once. The bytes are trusted (see the module docs):
/// neither the fetcher nor the engine hashes them.
pub struct PeerFetcher {
    addr: String,
    health: Arc<PeerHealth>,
    requests: tokio::sync::mpsc::UnboundedSender<(ChunkHash, PeerWaiter)>,
}

impl PeerFetcher {
    /// Start the fetcher's batching task. It ends when the fetcher drops.
    pub fn spawn(addr: String, scope: PeerChunkScope, health: Arc<PeerHealth>) -> Arc<Self> {
        let (requests, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(batch_requests(addr.clone(), scope, health.clone(), rx));
        Arc::new(Self {
            addr,
            health,
            requests,
        })
    }
}

#[async_trait::async_trait]
impl engram_fill::PeerSource for PeerFetcher {
    async fn fetch(&self, hash: ChunkHash) -> Option<bytes::Bytes> {
        if self.health.is_lost(&self.addr) {
            return None;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.requests.send((hash, tx)).ok()?;
        rx.await.ok().flatten()
    }
}

/// A peer source that counts the chunks its peer answered.
pub struct CountingPeer {
    inner: Arc<dyn engram_fill::PeerSource>,
    answered: std::sync::atomic::AtomicU64,
}

impl CountingPeer {
    pub fn new(inner: Arc<dyn engram_fill::PeerSource>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            answered: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub fn answered(&self) -> u64 {
        self.answered.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl engram_fill::PeerSource for CountingPeer {
    async fn fetch(&self, hash: ChunkHash) -> Option<bytes::Bytes> {
        let got = self.inner.fetch(hash).await;
        if got.is_some() {
            self.answered
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        got
    }
}

async fn batch_requests(
    addr: String,
    scope: PeerChunkScope,
    health: Arc<PeerHealth>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<(ChunkHash, PeerWaiter)>,
) {
    let in_flight = Arc::new(Semaphore::new(PEER_BATCHES_IN_FLIGHT));
    let mut client: Option<GrpcHostClient> = None;
    while let Some(first) = rx.recv().await {
        let slot = Arc::clone(&in_flight)
            .acquire_owned()
            .await
            .expect("never closed");
        // Everything that queued while the last batch waited for a slot
        // goes in this one.
        let mut batch = vec![first];
        while batch.len() < PEER_BATCH {
            match rx.try_recv() {
                Ok(request) => batch.push(request),
                Err(_) => break,
            }
        }
        if health.is_lost(&addr) {
            continue; // dropping the waiters answers None
        }
        let connected = match client.clone() {
            Some(c) => c,
            None => match peer_endpoint(&addr) {
                Ok(endpoint) => match endpoint.connect().await {
                    Ok(channel) => {
                        let c = GrpcHostClient::new(channel);
                        client = Some(c.clone());
                        c
                    }
                    Err(e) => {
                        tracing::warn!(%addr, error = %e, "peer fetch: dial failed; peer lost for the window");
                        health.mark_lost(&addr);
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!(%addr, error = %e, "peer fetch: bad endpoint");
                    health.mark_lost(&addr);
                    continue;
                }
            },
        };
        let (scope, health, addr) = (scope.clone(), health.clone(), addr.clone());
        tokio::spawn(async move {
            let _slot = slot;
            fetch_batch(connected, scope, batch, &health, &addr).await;
        });
    }
}

/// One `PeerChunkGet` stream for `batch`. A chunk the peer sends whole is
/// answered with its bytes; a missing chunk, and every chunk still open
/// when the stream fails, is answered `None` (its waiter is dropped).
async fn fetch_batch(
    client: GrpcHostClient,
    scope: PeerChunkScope,
    batch: Vec<(ChunkHash, PeerWaiter)>,
    health: &PeerHealth,
    addr: &str,
) {
    use futures::StreamExt;
    let wire: Vec<[u8; 32]> = batch.iter().map(|(h, _)| *h.as_bytes()).collect();
    let mut waiters: Vec<Option<PeerWaiter>> = batch.into_iter().map(|(_, w)| Some(w)).collect();
    let mut stream = match client.peer_chunk_get(wire, scope).await {
        Ok(s) => s,
        Err(engram_core::SandboxError::LimitExceeded(_)) => {
            metrics::counter!("engram_peer_pull_total", "outcome" => "backpressure").increment(1);
            return;
        }
        Err(e) => {
            tracing::warn!(addr, error = %e, "peer fetch: stream open failed; peer lost for the window");
            health.mark_lost(addr);
            return;
        }
    };
    let mut reassembly = Reassembly::new(waiters.len());
    while let Some(frame) = stream.next().await {
        let assembled = match frame {
            Ok(frame) => reassembly.push(frame),
            Err(e) => Err(e.to_string()),
        };
        match assembled {
            Ok(Assembled::Partial) => {}
            Ok(Assembled::Missing(idx)) => drop(waiters[idx].take()),
            Ok(Assembled::Whole(idx, body)) => {
                if let Some(waiter) = waiters[idx].take() {
                    let _ = waiter.send(Some(bytes::Bytes::from(body)));
                    metrics::counter!(CHUNK_FILL_TOTAL, "source" => "peer").increment(1);
                }
            }
            Err(e) => {
                tracing::warn!(addr, error = %e, "peer fetch: stream failed; peer lost for the window");
                health.mark_lost(addr);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(item_idx: u32, offset: u64, data: &[u8], last: bool) -> PeerChunkFrame {
        PeerChunkFrame {
            item_idx,
            offset,
            data: data.to_vec(),
            last,
            missing: false,
            crc32c: crc32c::crc32c(data),
        }
    }

    fn missing(item_idx: u32) -> PeerChunkFrame {
        PeerChunkFrame {
            item_idx,
            missing: true,
            last: true,
            ..PeerChunkFrame::default()
        }
    }

    #[test]
    fn in_order_frames_make_whole_items() {
        let mut r = Reassembly::new(3);
        assert_eq!(r.push(frame(0, 0, b"ab", false)), Ok(Assembled::Partial));
        assert_eq!(
            r.push(frame(0, 2, b"cd", true)),
            Ok(Assembled::Whole(0, b"abcd".to_vec()))
        );
        assert_eq!(r.push(missing(1)), Ok(Assembled::Missing(1)));
        assert_eq!(
            r.push(frame(2, 0, b"", true)),
            Ok(Assembled::Whole(2, Vec::new()))
        );
    }

    #[test]
    fn a_frame_out_of_order_fails_the_answer() {
        // The second half of item 0 first.
        let mut r = Reassembly::new(1);
        assert!(r.push(frame(0, 2, b"cd", true)).is_err());

        // A repeated frame.
        let mut r = Reassembly::new(1);
        r.push(frame(0, 0, b"ab", false)).unwrap();
        assert!(r.push(frame(0, 0, b"ab", true)).is_err());

        // A frame skipped.
        let mut r = Reassembly::new(1);
        r.push(frame(0, 0, b"ab", false)).unwrap();
        assert!(r.push(frame(0, 4, b"ef", true)).is_err());
    }

    #[test]
    fn interleaved_items_fail_the_answer() {
        let mut r = Reassembly::new(2);
        r.push(frame(0, 0, b"ab", false)).unwrap();
        assert!(r.push(frame(1, 0, b"xy", true)).is_err());

        let mut r = Reassembly::new(2);
        r.push(frame(0, 0, b"ab", false)).unwrap();
        assert!(r.push(missing(0)).is_err());
    }

    #[test]
    fn a_bad_crc_or_unknown_item_fails_the_answer() {
        let mut r = Reassembly::new(1);
        let mut bad = frame(0, 0, b"ab", true);
        bad.crc32c ^= 1;
        assert!(r.push(bad).is_err());

        let mut r = Reassembly::new(1);
        assert!(r.push(frame(1, 0, b"ab", true)).is_err());
        assert!(Reassembly::new(1).push(missing(1)).is_err());
    }
}
