//! ADR 0045 C2: the destination handler's side of the post-copy page
//! channel — `PeerSession` (TCP client to the SOURCE host-agent's page
//! server, `engram-migrate-proto` framing) and `ControlTx` (the one-way
//! `--control-sock` UDS the host-agent listens on).
//!
//! Deliberately minimal and fully synchronous: the fault loop is a
//! blocking thread, and this client must never grow tonic/tokio. One
//! connection serves the fault path (single in-flight request — the
//! fault loop serves one fault at a time, so pipelining buys nothing
//! there); the background drain opens its own connection(s) and
//! pipelines.
//!
//! ## Lifecycle / soundness
//!
//! `PeerSession::connect` returns only once the `Seal` bitmap is held
//! (the server pushes it after `HelloAck`; for pre-staged destinations
//! the server parks the `Hello` until the source's capture registers
//! the export). `main.rs` connects BEFORE binding the FC-facing UDS, so
//! "FC can load" implies "seal held" — the handler never serves a fault
//! it can't classify, without trusting external ordering.
//!
//! A sha mismatch on `Page` bytes is FATAL (peer-authoritative content
//! has no second source). Connection failures get a bounded reconnect
//! (the export stays open on the source under its TTL; requests are
//! idempotent reads of a frozen address space); exhaustion latches the
//! session lost — the caller reports `PeerLost` and the fault loop
//! exits loud rather than papering over missing state.

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use engram_migrate_proto::{
    read_frame, write_frame, FromSource, HandlerControl, SealBitmap, ToSource, PROTO_VERSION,
};

/// Reconnect policy: the source export outlives transient dials (its
/// TTL is ~120 s of silence), but a dead source must surface fast —
/// every sealed chunk we can't pull is a wedged vCPU.
const RECONNECT_ATTEMPTS: u32 = 3;
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// Errors on the peer channel. `ShaMismatch` and exhausted reconnects
/// are terminal: the caller latches `lost` and reports `PeerLost`.
#[derive(Debug)]
pub enum PeerError {
    Io(std::io::Error),
    /// The server answered `Error` (protocol misuse or serving failure).
    Server(String),
    /// `Page` bytes didn't hash to the carried sha256 — corrupt or
    /// hostile peer; never installable.
    ShaMismatch {
        chunk_offset: u64,
    },
    /// HelloAck/Seal geometry doesn't match the session manifest —
    /// manifest skew would make ALT_SOURCE unsound; refuse loudly.
    GeometryMismatch(String),
    /// Reconnect attempts exhausted with the session still needed.
    Lost(String),
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "peer io: {e}"),
            Self::Server(m) => write!(f, "peer server error: {m}"),
            Self::ShaMismatch { chunk_offset } => {
                write!(f, "peer page sha mismatch at offset {chunk_offset:#x}")
            }
            Self::GeometryMismatch(m) => write!(f, "peer geometry mismatch: {m}"),
            Self::Lost(m) => write!(f, "peer lost: {m}"),
        }
    }
}

impl std::error::Error for PeerError {}

impl From<std::io::Error> for PeerError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// One sealed chunk's resolution from the peer.
#[derive(Debug)]
pub enum PeerPage {
    /// Peer-authoritative bytes, sha-verified before return.
    Bytes(Vec<u8>),
    /// Whole chunk is zeros — install via the zero path.
    Zero,
    /// Live content equals the durable manifest entry: serve through
    /// the normal class-2 `resolve()` path instead.
    AltSource([u8; 32]),
}

/// Drain accounting (rides `ToSource::DrainDone` + the control sock).
#[derive(Clone, Copy, Debug, Default)]
pub struct DrainStats {
    pub pulled: u64,
    pub alt_sourced: u64,
    pub zero_chunks: u64,
}

#[derive(Debug)]
pub struct PeerSession {
    addr: String,
    export_id: String,
    token: String,
    seal: SealBitmap,
    chunk_size: u64,
    expected_total: u64,
    /// The fault-path connection. Single in-flight by construction
    /// (the fault loop is single-threaded); the mutex also covers the
    /// reconnect-swap.
    fault_conn: Mutex<TcpStream>,
    next_req: AtomicU64,
    lost: AtomicBool,
    /// Fault-path accounting (restore-tail attribution): how many
    /// guest faults round-tripped the peer and their cumulative wall —
    /// the serial P2P cost inside the FC load + early execution.
    fault_count: AtomicU64,
    fault_us: AtomicU64,
    fault_max_us: AtomicU64,
    /// Fault-priority gate for the drain: guest faults in flight +
    /// the µs-since-`epoch` stamp of the last fault completion. The
    /// drain parks while a fault is active-or-recent so the fault
    /// never queues behind the drain's bulk bytes on the wire (the
    /// measured ~7 ms/fault was exactly that queueing).
    faults_inflight: AtomicU64,
    last_fault_us: AtomicU64,
    epoch: std::time::Instant,
}

/// Dial + `Hello` + `HelloAck` + `Seal` on a fresh connection,
/// validating geometry against the session manifest's view.
fn dial(
    addr: &str,
    export_id: &str,
    token: &str,
    expect_chunk_size: u64,
    expect_total_bytes: u64,
    purpose: engram_migrate_proto::ConnPurpose,
) -> Result<(TcpStream, SealBitmap), PeerError> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    write_frame(
        &mut stream,
        &ToSource::Hello {
            version: PROTO_VERSION,
            export_id: export_id.to_string(),
            token: token.to_string(),
            purpose,
        },
    )?;
    match read_frame::<_, FromSource>(&mut stream)? {
        FromSource::HelloAck {
            version,
            chunk_size,
            total_bytes,
        } => {
            if version != PROTO_VERSION {
                return Err(PeerError::GeometryMismatch(format!(
                    "proto version {version} != {PROTO_VERSION}"
                )));
            }
            if chunk_size != expect_chunk_size || total_bytes != expect_total_bytes {
                return Err(PeerError::GeometryMismatch(format!(
                    "source serves chunk_size={chunk_size}/total={total_bytes}, \
                     manifest says {expect_chunk_size}/{expect_total_bytes}"
                )));
            }
        }
        FromSource::Error { message, .. } => return Err(PeerError::Server(message)),
        other => {
            return Err(PeerError::Server(format!(
                "expected HelloAck, got {other:?}"
            )));
        }
    }
    let seal = match read_frame::<_, FromSource>(&mut stream)? {
        FromSource::Seal { bitmap } => bitmap,
        FromSource::Error { message, .. } => return Err(PeerError::Server(message)),
        other => return Err(PeerError::Server(format!("expected Seal, got {other:?}"))),
    };
    seal.validate().map_err(PeerError::GeometryMismatch)?;
    if seal.chunk_size != expect_chunk_size
        || seal.chunk_count != expect_total_bytes.div_ceil(expect_chunk_size)
    {
        return Err(PeerError::GeometryMismatch(
            "seal bitmap geometry != manifest geometry".into(),
        ));
    }
    Ok((stream, seal))
}

impl PeerSession {
    /// Connect, authenticate, and BLOCK until the source's `Seal`
    /// arrives. Geometry is validated against the manifest's view
    /// before any fault can be served.
    pub fn connect(
        addr: String,
        export_id: String,
        token: String,
        expect_chunk_size: u64,
        expect_total_bytes: u64,
    ) -> Result<Self, PeerError> {
        let (stream, seal) = dial(
            &addr,
            &export_id,
            &token,
            expect_chunk_size,
            expect_total_bytes,
            engram_migrate_proto::ConnPurpose::Fault,
        )?;
        tracing::info!(
            addr,
            export_id,
            sealed_chunks = seal.count_ones(),
            total_chunks = seal.chunk_count,
            "peer session sealed"
        );
        Ok(Self {
            addr,
            export_id,
            token,
            seal,
            chunk_size: expect_chunk_size,
            expected_total: expect_total_bytes,
            fault_conn: Mutex::new(stream),
            next_req: AtomicU64::new(1),
            lost: AtomicBool::new(false),
            fault_count: AtomicU64::new(0),
            fault_us: AtomicU64::new(0),
            fault_max_us: AtomicU64::new(0),
            faults_inflight: AtomicU64::new(0),
            last_fault_us: AtomicU64::new(0),
            epoch: std::time::Instant::now(),
        })
    }

    /// Fault-path totals: `(count, cumulative_µs, max_µs)`.
    pub fn fault_stats(&self) -> (u64, u64, u64) {
        (
            self.fault_count.load(Ordering::Relaxed),
            self.fault_us.load(Ordering::Relaxed),
            self.fault_max_us.load(Ordering::Relaxed),
        )
    }

    /// True while a guest fault is in flight or one completed within
    /// `window` — the drain's park condition.
    pub fn fault_active_within(&self, window: std::time::Duration) -> bool {
        if self.faults_inflight.load(Ordering::Relaxed) > 0 {
            return true;
        }
        let last = self.last_fault_us.load(Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        let now = self.epoch.elapsed().as_micros() as u64;
        now.saturating_sub(last) < window.as_micros() as u64
    }

    /// The sealed dirty map (held from construction — no waiting).
    pub fn seal(&self) -> &SealBitmap {
        &self.seal
    }

    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    pub fn mark_lost(&self) {
        self.lost.store(true, Ordering::SeqCst);
    }

    /// Open an additional authenticated connection (the drain path).
    /// The redundant `Seal` push is read and discarded by `dial`.
    pub fn open_extra_conn(&self) -> Result<TcpStream, PeerError> {
        let (stream, _seal) = dial(
            &self.addr,
            &self.export_id,
            &self.token,
            self.chunk_size,
            self.expected_total,
            engram_migrate_proto::ConnPurpose::Drain,
        )?;
        Ok(stream)
    }

    /// Fault-path request: one round-trip on the shared connection,
    /// sha-verified. Reconnects through `RECONNECT_ATTEMPTS` before
    /// declaring the peer lost.
    pub fn need_at(&self, chunk_offset: u64) -> Result<PeerPage, PeerError> {
        if self.is_lost() {
            return Err(PeerError::Lost("peer already marked lost".into()));
        }
        let _fault = self.begin_fault();
        let mut conn = self.fault_conn.lock().expect("fault conn poisoned");
        let mut last_err: Option<PeerError> = None;
        for attempt in 0..=RECONNECT_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(RECONNECT_BACKOFF);
                match dial(
                    &self.addr,
                    &self.export_id,
                    &self.token,
                    self.chunk_size,
                    self.expected_total,
                    engram_migrate_proto::ConnPurpose::Fault,
                ) {
                    Ok((fresh, _seal)) => *conn = fresh,
                    Err(e) => {
                        tracing::warn!(attempt, error = %e, "peer redial failed");
                        last_err = Some(e);
                        continue;
                    }
                }
            }
            match request_chunk(&mut conn, &self.next_req, chunk_offset) {
                Ok(page) => return Ok(page),
                // Terminal classifications never retry.
                Err(e @ PeerError::ShaMismatch { .. })
                | Err(e @ PeerError::Server(_))
                | Err(e @ PeerError::GeometryMismatch(_)) => {
                    self.mark_lost();
                    return Err(e);
                }
                Err(e) => {
                    tracing::warn!(attempt, error = %e, "peer request failed; will redial");
                    last_err = Some(e);
                }
            }
        }
        self.mark_lost();
        Err(PeerError::Lost(format!(
            "reconnects exhausted: {}",
            last_err.map(|e| e.to_string()).unwrap_or_default()
        )))
    }
}

impl PeerSession {
    /// Fault-scope guard: counts the fault, marks it in-flight (the
    /// drain's park condition), and on drop — every return path —
    /// accumulates the elapsed wall, tracks the max, and stamps the
    /// completion time for the drain's recent-fault window.
    fn begin_fault(&self) -> impl Drop + '_ {
        self.fault_count.fetch_add(1, Ordering::Relaxed);
        self.faults_inflight.fetch_add(1, Ordering::Relaxed);
        struct G<'a>(&'a PeerSession, std::time::Instant);
        impl Drop for G<'_> {
            fn drop(&mut self) {
                let us = self.1.elapsed().as_micros() as u64;
                self.0.fault_us.fetch_add(us, Ordering::Relaxed);
                self.0.fault_max_us.fetch_max(us, Ordering::Relaxed);
                self.0.faults_inflight.fetch_sub(1, Ordering::Relaxed);
                self.0
                    .last_fault_us
                    .store(self.0.epoch.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
        }
        G(self, std::time::Instant::now())
    }
}

/// One `NeedAt` round-trip on `conn` (used by the fault path and, with
/// its own connection, the drain).
pub fn request_chunk(
    conn: &mut TcpStream,
    next_req: &AtomicU64,
    chunk_offset: u64,
) -> Result<PeerPage, PeerError> {
    let req_id = next_req.fetch_add(1, Ordering::Relaxed);
    write_frame(
        conn,
        &ToSource::NeedAt {
            req_id,
            chunk_offset,
        },
    )?;
    decode_page(read_frame::<_, FromSource>(conn)?, req_id, chunk_offset)
}

/// Decode + verify one `NeedAt` response.
pub fn decode_page(
    resp: FromSource,
    want_req: u64,
    want_offset: u64,
) -> Result<PeerPage, PeerError> {
    match resp {
        FromSource::Page {
            req_id,
            chunk_offset,
            bytes,
            hash,
            lz4,
        } => {
            if req_id != want_req || chunk_offset != want_offset {
                return Err(PeerError::Server(format!(
                    "response mismatch: req {req_id}/{want_req}, offset \
                     {chunk_offset:#x}/{want_offset:#x}"
                )));
            }
            // Integrity covers the WIRE bytes; decompress only after.
            if engram_migrate_proto::wire_hash(&bytes) != hash {
                return Err(PeerError::ShaMismatch {
                    chunk_offset: want_offset,
                });
            }
            let raw = engram_migrate_proto::decompress_page(bytes, lz4)
                // Verified-but-undecompressable = the source is
                // serving garbage; terminal, same class as a sha
                // mismatch.
                .map_err(PeerError::Server)?;
            Ok(PeerPage::Bytes(raw))
        }
        FromSource::ZeroChunk { req_id, .. } if req_id == want_req => Ok(PeerPage::Zero),
        FromSource::AltSource {
            req_id,
            durable_sha256,
            ..
        } if req_id == want_req => Ok(PeerPage::AltSource(durable_sha256)),
        FromSource::Error { message, .. } => Err(PeerError::Server(message)),
        other => Err(PeerError::Server(format!("unexpected response: {other:?}"))),
    }
}

/// Send `DrainDone` on `conn` (the drain's own connection).
pub fn send_drain_done(conn: &mut TcpStream, stats: DrainStats) -> Result<(), PeerError> {
    write_frame(
        conn,
        &ToSource::DrainDone {
            pulled: stats.pulled,
            alt_sourced: stats.alt_sourced,
            zero_chunks: stats.zero_chunks,
        },
    )?;
    Ok(())
}

// ---- Control socket -----------------------------------------------------

/// The handler's side of `--control-sock`: binds a UnixListener, lets
/// the host-agent dial in, and streams `HandlerControl` frames one-way.
/// New subscribers get the backlog replayed (the host-agent may dial
/// after `Sealed` fired). Reports NEVER block the caller: writes carry
/// a short timeout and a dead subscriber is dropped.
pub struct ControlTx {
    inner: std::sync::Arc<ControlInner>,
}

struct ControlInner {
    // LOCK ORDER (invariant): always acquire `backlog` BEFORE `subscribers`,
    // and on both the subscribe path and the report path hold `backlog`
    // across the `subscribers` critical section. This makes "replay the
    // backlog + register the subscriber" and "append to the backlog + fan
    // out to subscribers" atomic with respect to each other: a frame can
    // never land between a fresh subscriber's backlog snapshot and its
    // registration (the lost-terminal-frame race, issue #207). NEVER take
    // `subscribers` first or hold it across a `backlog` acquisition.
    subscribers: Mutex<Vec<std::os::unix::net::UnixStream>>,
    backlog: Mutex<Vec<HandlerControl>>,
    // Test-only seam: lets a regression test force-open the gap between
    // the backlog snapshot and the subscriber registration. In production
    // this is `None` and the closure is never run.
    #[cfg(test)]
    on_replay_done: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl ControlInner {
    /// Replay the backlog to a freshly-dialed subscriber and register it
    /// for live frames — atomically w.r.t. [`ControlInner::report`].
    ///
    /// The `backlog` lock is the single linearization point: it is held
    /// across BOTH the replay (to the not-yet-shared `stream`) and the
    /// push into `subscribers`, so no `report()` can interleave to land a
    /// frame in neither the replay nor the live fan-out. The replay
    /// writes are bounded by the per-connection 1 s write timeout, so the
    /// widened critical section cannot stall unboundedly.
    fn add_subscriber(&self, mut stream: std::os::unix::net::UnixStream) {
        let backlog = self.backlog.lock().expect("backlog poisoned");
        let mut ok = true;
        for msg in backlog.iter() {
            if write_frame(&mut stream, msg).is_err() {
                ok = false;
                break;
            }
        }
        // Test-only: run AFTER the snapshot/replay but BEFORE registering
        // the subscriber, while still holding `backlog`. A concurrent
        // `report()` will block on `backlog` here — proving the
        // serialization (the frame is delivered via replay on the next
        // dial OR via live fan-out once we register, never neither).
        #[cfg(test)]
        if let Some(cb) = self.on_replay_done.lock().expect("hook poisoned").as_ref() {
            cb();
        }
        if ok {
            self.subscribers
                .lock()
                .expect("subscribers poisoned")
                .push(stream);
        }
        // `backlog` lock released here, after the subscriber is live.
    }

    /// Record + fan out one report under the backlog lock (see lock-order
    /// invariant on the fields). Never blocks beyond the per-write timeout;
    /// dead subscribers are pruned.
    fn report(&self, msg: HandlerControl) {
        let mut backlog = self.backlog.lock().expect("backlog poisoned");
        backlog.push(msg.clone());
        let mut subs = self.subscribers.lock().expect("subscribers poisoned");
        subs.retain_mut(|s| write_frame(s, &msg).is_ok());
        // Both locks released here (subs first, then backlog).
    }
}

impl ControlTx {
    pub fn bind(path: &Path) -> std::io::Result<Self> {
        let _ = std::fs::remove_file(path);
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        let inner = std::sync::Arc::new(ControlInner {
            subscribers: Mutex::new(Vec::new()),
            backlog: Mutex::new(Vec::new()),
            #[cfg(test)]
            on_replay_done: Mutex::new(None),
        });
        let accept_inner = std::sync::Arc::clone(&inner);
        let path_owned: PathBuf = path.to_path_buf();
        std::thread::Builder::new()
            .name("engram-uffd-control".to_string())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                    // Replay the backlog so a late subscriber still sees
                    // Sealed/DrainDone, then subscribe for live frames —
                    // atomically w.r.t. report() (issue #207).
                    accept_inner.add_subscriber(stream);
                }
                tracing::debug!(path = %path_owned.display(), "control listener exited");
            })?;
        Ok(Self { inner })
    }

    /// Record + fan out one report. Never blocks beyond the per-write
    /// timeout; dead subscribers are pruned.
    pub fn report(&self, msg: HandlerControl) {
        self.inner.report(msg);
    }

    /// Test-only: install a hook fired between a subscriber's backlog
    /// snapshot and its registration (while the backlog lock is held).
    #[cfg(test)]
    fn set_replay_hook(&self, cb: Box<dyn Fn() + Send>) {
        *self.inner.on_replay_done.lock().expect("hook poisoned") = Some(cb);
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    const CHUNK: u64 = 4096;
    const TOTAL: u64 = 4 * 4096;

    /// A minimal fake source: accepts conns, answers Hello/Ack/Seal,
    /// then serves canned NeedAt responses.
    fn fake_source(
        seal_bits: Vec<u64>,
        respond: impl Fn(u64, u64) -> FromSource + Send + Clone + 'static,
    ) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let respond = respond.clone();
                let seal_bits = seal_bits.clone();
                std::thread::spawn(move || {
                    let Ok(ToSource::Hello { token, .. }) = read_frame::<_, ToSource>(&mut s)
                    else {
                        return;
                    };
                    if token != "tok" {
                        let _ = write_frame(
                            &mut s,
                            &FromSource::Error {
                                req_id: None,
                                message: "bad token".into(),
                            },
                        );
                        return;
                    }
                    write_frame(
                        &mut s,
                        &FromSource::HelloAck {
                            version: PROTO_VERSION,
                            chunk_size: CHUNK,
                            total_bytes: TOTAL,
                        },
                    )
                    .unwrap();
                    let mut bitmap = SealBitmap::new(CHUNK, TOTAL / CHUNK);
                    for b in &seal_bits {
                        bitmap.set(*b);
                    }
                    write_frame(&mut s, &FromSource::Seal { bitmap }).unwrap();
                    while let Ok(req) = read_frame::<_, ToSource>(&mut s) {
                        match req {
                            ToSource::NeedAt {
                                req_id,
                                chunk_offset,
                            } => {
                                let resp = respond(req_id, chunk_offset);
                                if write_frame(&mut s, &resp).is_err() {
                                    return;
                                }
                            }
                            ToSource::DrainDone { .. } => return,
                            _ => return,
                        }
                    }
                });
            }
        });
        addr
    }

    fn page_resp(req_id: u64, chunk_offset: u64) -> FromSource {
        // Through the REAL compression path — the canned server
        // serves exactly what the prod source serves.
        let (bytes, lz4) = engram_migrate_proto::compress_page(vec![0xAB; CHUNK as usize]);
        let hash = engram_migrate_proto::wire_hash(&bytes);
        FromSource::Page {
            req_id,
            chunk_offset,
            bytes,
            hash,
            lz4,
        }
    }

    #[test]
    fn connect_validates_geometry_and_holds_seal() {
        let addr = fake_source(vec![0, 2], page_resp);
        let sess = PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK, TOTAL)
            .expect("connect");
        assert!(sess.seal().get(0) && !sess.seal().get(1) && sess.seal().get(2));

        // Wrong geometry expectation → loud refusal.
        let err =
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK * 2, TOTAL)
                .expect_err("geometry mismatch must refuse");
        assert!(matches!(err, PeerError::GeometryMismatch(_)), "{err}");

        // Bad token → server error surfaces.
        let err = PeerSession::connect(addr.to_string(), "e".into(), "bad".into(), CHUNK, TOTAL)
            .expect_err("bad token must refuse");
        assert!(matches!(err, PeerError::Server(_)), "{err}");
    }

    #[test]
    fn need_at_verifies_sha_and_decodes_all_arms() {
        let addr = fake_source(
            vec![0, 1, 2, 3],
            |req_id, chunk_offset| match chunk_offset {
                0 => page_resp(req_id, 0),
                o if o == CHUNK => FromSource::ZeroChunk {
                    req_id,
                    chunk_offset: o,
                },
                o if o == 2 * CHUNK => FromSource::AltSource {
                    req_id,
                    chunk_offset: o,
                    durable_sha256: [0x5A; 32],
                },
                o => {
                    // Corrupt page: bytes don't match the sha.
                    let bytes = vec![0xFF; CHUNK as usize];
                    FromSource::Page {
                        req_id,
                        chunk_offset: o,
                        bytes,
                        hash: [0u8; 32],
                        lz4: false,
                    }
                }
            },
        );
        let sess = PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK, TOTAL)
            .expect("connect");

        match sess.need_at(0).expect("page") {
            PeerPage::Bytes(b) => assert!(b.iter().all(|x| *x == 0xAB)),
            other => panic!("expected Bytes, got {other:?}"),
        }
        assert!(matches!(sess.need_at(CHUNK), Ok(PeerPage::Zero)));
        match sess.need_at(2 * CHUNK) {
            Ok(PeerPage::AltSource(h)) => assert_eq!(h, [0x5A; 32]),
            other => panic!("expected AltSource, got {other:?}"),
        }
        // Sha mismatch is terminal: error + session latched lost.
        let err = sess.need_at(3 * CHUNK).expect_err("sha mismatch");
        assert!(matches!(err, PeerError::ShaMismatch { .. }), "{err}");
        assert!(sess.is_lost());
        // Subsequent requests refuse immediately.
        assert!(matches!(sess.need_at(0), Err(PeerError::Lost(_))));
    }

    #[test]
    fn need_at_reconnects_through_a_dropped_connection() {
        // A server that drops the FIRST NeedAt connection mid-request,
        // then serves normally on redials.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut first = true;
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let drop_after_seal = first;
                first = false;
                std::thread::spawn(move || {
                    let Ok(ToSource::Hello { .. }) = read_frame::<_, ToSource>(&mut s) else {
                        return;
                    };
                    write_frame(
                        &mut s,
                        &FromSource::HelloAck {
                            version: PROTO_VERSION,
                            chunk_size: CHUNK,
                            total_bytes: TOTAL,
                        },
                    )
                    .unwrap();
                    let mut bitmap = SealBitmap::new(CHUNK, TOTAL / CHUNK);
                    bitmap.set(0);
                    write_frame(&mut s, &FromSource::Seal { bitmap }).unwrap();
                    if drop_after_seal {
                        return; // connection dies before serving
                    }
                    while let Ok(ToSource::NeedAt {
                        req_id,
                        chunk_offset,
                    }) = read_frame::<_, ToSource>(&mut s)
                    {
                        if write_frame(&mut s, &page_resp(req_id, chunk_offset)).is_err() {
                            return;
                        }
                    }
                });
            }
        });

        let sess = PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK, TOTAL)
            .expect("connect");
        match sess.need_at(0).expect("served after reconnect") {
            PeerPage::Bytes(b) => assert!(b.iter().all(|x| *x == 0xAB)),
            other => panic!("expected Bytes, got {other:?}"),
        }
        assert!(!sess.is_lost());
    }

    #[test]
    fn control_tx_replays_backlog_and_streams() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let tx = ControlTx::bind(&path).unwrap();

        // Report BEFORE any subscriber: lands in the backlog.
        tx.report(HandlerControl::Sealed {
            dirty_chunks: 3,
            total_chunks: 4,
            at_unix_ms: 1,
        });

        let mut sub = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let got: HandlerControl = read_frame(&mut sub).unwrap();
        assert!(matches!(
            got,
            HandlerControl::Sealed {
                dirty_chunks: 3,
                ..
            }
        ));

        // Live frame after subscription: now deterministic (the subscribe
        // path and report() serialize on the backlog lock), so a single
        // report must arrive — no poll-retry crutch.
        tx.report(HandlerControl::DrainProgress {
            pulled: 1,
            remaining: 2,
        });
        sub.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let got: HandlerControl = read_frame(&mut sub).unwrap();
        assert!(
            matches!(got, HandlerControl::DrainProgress { .. }),
            "live control frame never arrived: {got:?}"
        );
    }

    /// Regression for issue #207: a terminal frame reported in the gap
    /// between a subscriber's backlog snapshot and its registration must
    /// still be delivered exactly once (never lost). Deterministic via a
    /// test-only hook that holds the backlog lock open across a concurrent
    /// `report()`, which is exactly the racy interleaving that used to drop
    /// `DrainDone`/`PeerLost` and stall `migration_drain_wait` for 600 s.
    ///
    /// Pre-fix (snapshot under lock, RELEASE, replay, then a separate
    /// registration) this report landed in neither the replay (snapshot
    /// already taken) nor the live fan-out (subscriber not yet registered)
    /// and the read below would time out.
    #[test]
    fn control_tx_no_lost_frame_during_subscribe_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let tx = std::sync::Arc::new(ControlTx::bind(&path).unwrap());

        // Two single-fire channels: the accept thread (inside the hook,
        // holding the backlog lock) signals it has entered the gap; the
        // reporter then fires a terminal frame which MUST block on the
        // backlog lock until the subscriber is registered.
        let (in_gap_tx, in_gap_rx) = std::sync::mpsc::channel::<()>();
        let in_gap_tx = std::sync::Mutex::new(Some(in_gap_tx));
        let tx_for_report = std::sync::Arc::clone(&tx);
        tx.set_replay_hook(Box::new(move || {
            // We hold the backlog lock here (post-snapshot, pre-register).
            if let Some(s) = in_gap_tx.lock().unwrap().take() {
                let _ = s.send(());
                // Spawn the racing report and give it time to reach (and
                // block on) the backlog lock before we return / register.
                let txr = std::sync::Arc::clone(&tx_for_report);
                std::thread::spawn(move || {
                    txr.report(HandlerControl::DrainDone {
                        pulled: 7,
                        alt_sourced: 0,
                        zero_chunks: 0,
                        ms: 0,
                        faults: 0,
                        fault_us: 0,
                        fault_max_us: 0,
                    });
                });
                std::thread::sleep(Duration::from_millis(200));
            }
        }));

        let mut sub = std::os::unix::net::UnixStream::connect(&path).unwrap();
        // The hook fired on the accept thread; wait until we know we were
        // in the gap (so the test is meaningfully exercising the race).
        in_gap_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("accept thread never entered the subscribe gap");

        // The DrainDone reported during the gap must arrive — via live
        // fan-out once the now-registered subscriber is visible to report.
        sub.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let got: HandlerControl =
            read_frame(&mut sub).expect("terminal frame lost in subscribe gap");
        assert!(
            matches!(got, HandlerControl::DrainDone { pulled: 7, .. }),
            "unexpected frame: {got:?}"
        );

        // And exactly once: no duplicate (it was NOT also in the replay).
        sub.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        match read_frame::<_, HandlerControl>(&mut sub) {
            Err(_) => {} // timed out / EOF — good, no duplicate
            Ok(extra) => panic!("terminal frame delivered more than once: {extra:?}"),
        }
    }

    /// No deadlock under concurrent dial/report stress; every subscriber
    /// that registers before the final report sees it.
    #[test]
    fn control_tx_concurrent_dial_report_no_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        let tx = std::sync::Arc::new(ControlTx::bind(&path).unwrap());

        let reporter = {
            let tx = std::sync::Arc::clone(&tx);
            std::thread::spawn(move || {
                for i in 0..2000u64 {
                    tx.report(HandlerControl::DrainProgress {
                        pulled: i,
                        remaining: 0,
                    });
                }
                tx.report(HandlerControl::DrainDone {
                    pulled: 2000,
                    alt_sourced: 0,
                    zero_chunks: 0,
                    ms: 0,
                    faults: 0,
                    fault_us: 0,
                    fault_max_us: 0,
                });
            })
        };

        let mut dialers = Vec::new();
        for _ in 0..16 {
            let path = path.clone();
            dialers.push(std::thread::spawn(move || {
                let mut sub = std::os::unix::net::UnixStream::connect(&path).unwrap();
                sub.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                // Drain until DrainDone or EOF; must not block forever.
                loop {
                    match read_frame::<_, HandlerControl>(&mut sub) {
                        Ok(HandlerControl::DrainDone { .. }) => break,
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }));
        }

        reporter.join().unwrap();
        for d in dialers {
            d.join().unwrap();
        }
    }
}
