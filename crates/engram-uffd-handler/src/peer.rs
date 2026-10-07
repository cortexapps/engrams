//! ADR 0045 C2: the destination handler's side of the post-copy page
//! channel — `PeerSession` (TCP client to the SOURCE host-agent's page
//! server, `engram-migrate-proto` framing) and `ControlTx` (the one-way
//! `--control-sock` UDS the host-agent listens on).
//!
//! Deliberately minimal and fully synchronous: the fault loop is a
//! blocking thread, and this client must never grow tonic/tokio. One
//! connection serves the fault path (single in-flight request — the
//! fault loop requests one chunk at a time). Fault-around uses its own
//! worker connection; the background drain opens its own connections and pipelines.
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

use std::net::{TcpStream, ToSocketAddrs};
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

/// Page-channel socket deadlines. A silently-dead source (power loss /
/// partition with no FIN/RST) leaves the framing `read_exact` with no
/// unacked data to fail on, so a blocking read would hang forever — and
/// `need_at`/`drain` only ever surface `PeerLost` on an `Err` return.
/// These bound that wait so an `io::ErrorKind::WouldBlock`/`TimedOut`
/// arrives, converts to `PeerError::Io` (retryable), and the bounded
/// redial loop escalates to `Lost` within seconds instead of wedging a
/// vCPU (and teardown) on a dead peer. The 5 s read budget comfortably
/// exceeds the source's worst-case `process_vm_readv` service: it serves
/// from the PAUSED guest's resident RAM (no disk, no network), and the
/// drain's hot-first ordering keeps any single chunk's serve sub-ms.
pub(crate) const PEER_READ_TIMEOUT: Duration = Duration::from_secs(5);
const PEER_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// First response from a new worker: each attempt can connect, send Hello,
/// read HelloAck and Seal, send NeedWindow, then read the first page.
/// Include the initial attempt and every redial, plus reconnect backoff.
#[cfg(target_os = "linux")]
pub(crate) const WINDOW_FIRST_RESPONSE_TIMEOUT: Duration = PEER_READ_TIMEOUT
    .saturating_mul(4)
    .saturating_add(PEER_WRITE_TIMEOUT.saturating_mul(2))
    .saturating_mul(RECONNECT_ATTEMPTS + 1)
    .saturating_add(RECONNECT_BACKOFF.saturating_mul(RECONNECT_ATTEMPTS));

/// TCP keepalive so a dropped path with no in-flight data still surfaces
/// (the read/write timeouts only fire while a request is outstanding;
/// keepalive catches an idle fault conn between faults). Idle 10 s, then
/// probe every 5 s, 3 probes → the kernel drops the conn within ~25 s of
/// silence even when no read is pending, which `read_frame` then sees as
/// a connection error.
const PEER_KEEPALIVE_IDLE: Duration = Duration::from_secs(10);
const PEER_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
const PEER_KEEPALIVE_RETRIES: u32 = 3;

/// Errors on the peer channel. `ShaMismatch` and exhausted reconnects
/// are terminal: the caller latches `lost` and reports `PeerLost`.
#[derive(Debug)]
pub enum PeerError {
    Io(std::io::Error),
    /// The server answered `Error { req_id: None }` — a connection-fatal
    /// failure (bad hello, version skew, unknown export). Terminal: the
    /// connection cannot serve any request, so the caller latches `lost`.
    Server(String),
    /// The server answered `Error { req_id: Some }` — that *single*
    /// request failed (e.g. a transient `process_vm_readv` EAGAIN/ENOMEM
    /// under host memory pressure) but the connection stays alive and the
    /// next frame is served normally. Per the wire contract
    /// (`engram-migrate-proto` lib.rs: `Some` ⇒ "that request failed"),
    /// this is RETRYABLE — `need_at` redials/retries within
    /// `RECONNECT_ATTEMPTS` instead of declaring the peer lost.
    RequestFailed(String),
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
            Self::RequestFailed(m) => write!(f, "peer request failed (retryable): {m}"),
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

/// Default number of chunks in a demand window.
pub const DEFAULT_FAULT_AROUND_CHUNKS: usize = 8;

/// Select sealed, uninstalled chunks from the fault through this region.
/// The faulting chunk can straddle a region boundary; later chunks cannot.
pub fn fault_window(
    seal: &SealBitmap,
    first: u64,
    region_end: u64,
    limit: usize,
    installed: impl Fn(usize) -> bool,
) -> Vec<u64> {
    (first / seal.chunk_size..seal.chunk_count)
        .map(|idx| (idx, idx * seal.chunk_size))
        .take_while(|&(_, offset)| {
            offset == first || offset.saturating_add(seal.chunk_size) <= region_end
        })
        .filter(|&(idx, _)| seal.get(idx) && !installed(idx as usize))
        .take(limit)
        .map(|(_, offset)| offset)
        .collect()
}

/// Drain accounting (rides `ToSource::DrainDone` + the control sock).
#[derive(Clone, Copy, Debug, Default)]
pub struct DrainStats {
    pub hot_hint_received: bool,
    pub hot_leading: u64,
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

/// Bound the page channel against a silently-dead source: read/write
/// deadlines so the framing `read_exact`/`write_all` can never block
/// forever, plus TCP keepalive so an idle conn between faults still
/// surfaces a dropped path. Applied to EVERY dialed conn — the fault
/// conn (`connect` + the `need_at` redial) and the drain conn
/// (`open_extra_conn`) — since all route through `dial`.
fn apply_peer_sockopts(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(PEER_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(PEER_WRITE_TIMEOUT))?;
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(PEER_KEEPALIVE_IDLE)
        .with_interval(PEER_KEEPALIVE_INTERVAL)
        .with_retries(PEER_KEEPALIVE_RETRIES);
    // SockRef borrows the fd without taking ownership; the TcpStream
    // stays the owner.
    socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive)?;
    Ok(())
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
    dial_with_socket(
        addr,
        export_id,
        token,
        expect_chunk_size,
        expect_total_bytes,
        purpose,
        |_| Ok(true),
    )
}

/// `None` checks cancellation before and after each connect attempt. `Some`
/// registers the connected socket before the handshake can block.
fn connect_with_socket(
    addr: &str,
    connected: &impl Fn(Option<&TcpStream>) -> Result<bool, PeerError>,
) -> Result<TcpStream, PeerError> {
    if !connected(None)? {
        return Err(PeerError::Lost("connection canceled".into()));
    }
    let deadline = std::time::Instant::now() + PEER_READ_TIMEOUT;
    let mut last_error = std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "peer address resolved to no addresses",
    );
    for address in addr.to_socket_addrs()? {
        if !connected(None)? {
            return Err(PeerError::Lost("connection canceled".into()));
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
        }
        let result = TcpStream::connect_timeout(&address, remaining);
        if !connected(None)? {
            return Err(PeerError::Lost("connection canceled".into()));
        }
        match result {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = error,
        }
    }
    Err(last_error.into())
}

fn dial_with_socket(
    addr: &str,
    export_id: &str,
    token: &str,
    expect_chunk_size: u64,
    expect_total_bytes: u64,
    purpose: engram_migrate_proto::ConnPurpose,
    connected: impl Fn(Option<&TcpStream>) -> Result<bool, PeerError>,
) -> Result<(TcpStream, SealBitmap), PeerError> {
    let mut stream = connect_with_socket(addr, &connected)?;
    stream.set_nodelay(true)?;
    apply_peer_sockopts(&stream)?;
    // Register worker sockets before any blocking handshake read or write.
    if !connected(Some(&stream))? {
        return Err(PeerError::Lost("connection canceled".into()));
    }
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

    /// Test-only: the read/write deadlines actually set on the live
    /// fault connection (proves `apply_peer_sockopts` ran on the dialed
    /// stream, not just on a discarded local).
    #[cfg(test)]
    fn fault_conn_timeouts(&self) -> (Option<Duration>, Option<Duration>) {
        let conn = self.fault_conn.lock().expect("fault conn poisoned");
        (
            conn.read_timeout().expect("read_timeout"),
            conn.write_timeout().expect("write_timeout"),
        )
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

    /// Fault windows use their own connection; the source permits multiple
    /// Fault connections per export. This does not hold the drain priority gate.
    #[cfg(target_os = "linux")]
    pub(crate) fn open_prefetch_conn(
        &self,
        connected: impl Fn(Option<&TcpStream>) -> Result<bool, PeerError>,
    ) -> Result<TcpStream, PeerError> {
        dial_with_socket(
            &self.addr,
            &self.export_id,
            &self.token,
            self.chunk_size,
            self.expected_total,
            engram_migrate_proto::ConnPurpose::Fault,
            connected,
        )
        .map(|(stream, _)| stream)
    }

    /// Run a one-chunk demand window on the fault connection.
    pub fn need_at(&self, offset: u64) -> Result<PeerPage, PeerError> {
        let mut page = None;
        self.need_window(&[offset], |_, next| {
            page = Some(next);
            Ok::<_, std::convert::Infallible>(())
        })?
        .unwrap();
        Ok(page.expect("one response"))
    }

    /// Stream a window; retry only the suffix not yet installed.
    /// The outer error is a peer failure. The inner error is from the
    /// installer; it stops the stream and the caller must exit the handler.
    pub fn need_window<E>(
        &self,
        offsets: &[u64],
        install: impl FnMut(u64, PeerPage) -> Result<(), E>,
    ) -> Result<Result<(), E>, PeerError> {
        if self.is_lost() {
            return Err(PeerError::Lost("peer already marked lost".into()));
        }
        let _fault = self.begin_fault();
        let mut conn = self.fault_conn.lock().expect("fault conn poisoned");
        let result = self.stream_window(&mut conn, offsets, install, || false, |_| Ok(true));
        if result.is_err() {
            self.mark_lost();
        }
        result
    }

    /// Shared streaming and suffix-only retry logic. The prefetch caller owns
    /// its connection and handles errors under its cancellation lock, so a
    /// canceled window cannot latch the session lost after DrainDone.
    pub(crate) fn stream_window<E>(
        &self,
        conn: &mut TcpStream,
        offsets: &[u64],
        mut install: impl FnMut(u64, PeerPage) -> Result<(), E>,
        cancelled: impl Fn() -> bool,
        connected: impl Fn(Option<&TcpStream>) -> Result<bool, PeerError>,
    ) -> Result<Result<(), E>, PeerError> {
        let mut completed = 0;
        let mut last_err: Option<PeerError> = None;
        for attempt in 0..=RECONNECT_ATTEMPTS {
            if cancelled() {
                return Ok(Ok(()));
            }
            if attempt > 0 {
                std::thread::sleep(RECONNECT_BACKOFF);
                if cancelled() {
                    return Ok(Ok(()));
                }
                match dial_with_socket(
                    &self.addr,
                    &self.export_id,
                    &self.token,
                    self.chunk_size,
                    self.expected_total,
                    engram_migrate_proto::ConnPurpose::Fault,
                    &connected,
                ) {
                    Ok((fresh, _seal)) => *conn = fresh,
                    Err(e) => {
                        tracing::warn!(attempt, error = %e, "peer redial failed");
                        last_err = Some(e);
                        continue;
                    }
                }
            }
            if !connected(Some(conn))? {
                return Ok(Ok(()));
            }
            let result = (|| {
                let req_id = self.next_req.fetch_add(1, Ordering::Relaxed);
                write_frame(
                    &mut *conn,
                    &ToSource::NeedWindow {
                        req_id,
                        chunk_offsets: offsets[completed..].to_vec(),
                    },
                )?;
                while completed < offsets.len() {
                    let offset = offsets[completed];
                    let response = read_frame(&mut *conn)?;
                    if cancelled() {
                        return Ok(Ok(()));
                    }
                    let page = decode_page(response, req_id, offset)?;
                    if let Err(e) = install(offset, page) {
                        return Ok(Err(e));
                    }
                    completed += 1;
                }
                Ok(Ok(()))
            })();
            if cancelled() {
                return Ok(Ok(()));
            }
            match result {
                Ok(result) => return Ok(result),
                // Terminal classifications never retry: corrupt content, a
                // connection-fatal server error (`Error{req_id: None}`),
                // or geometry skew are all unrecoverable on any conn.
                Err(e @ PeerError::ShaMismatch { .. })
                | Err(e @ PeerError::Server(_))
                | Err(e @ PeerError::GeometryMismatch(_)) => {
                    return Err(e);
                }
                // Per-request failures (`Error{req_id: Some}`) and
                // connection-level IO are RETRYABLE: the wire contract
                // says `Some` means only THIS request failed and the conn
                // keeps serving. Redial+retry within RECONNECT_ATTEMPTS
                // rather than rewinding the whole migration on one
                // transient readv EAGAIN/ENOMEM. Only exhaustion (below)
                // latches the peer lost.
                Err(e @ PeerError::RequestFailed(_)) | Err(e @ PeerError::Io(_)) => {
                    tracing::warn!(attempt, error = %e, "peer request failed; will retry");
                    last_err = Some(e);
                }
                Err(e @ PeerError::Lost(_)) => {
                    // Should not surface from request_chunk, but treat as
                    // terminal if it ever does.
                    return Err(e);
                }
            }
        }
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
    pub(crate) fn begin_fault(&self) -> impl Drop + '_ {
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

/// Legacy single-chunk request, used to check one-chunk equivalence.
#[cfg(test)]
fn request_chunk(
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
        FromSource::ZeroChunk {
            req_id,
            chunk_offset,
        } if req_id == want_req && chunk_offset == want_offset => Ok(PeerPage::Zero),
        FromSource::AltSource {
            req_id,
            durable_sha256,
            chunk_offset,
        } if req_id == want_req && chunk_offset == want_offset => {
            Ok(PeerPage::AltSource(durable_sha256))
        }
        // Honor the wire contract: `req_id: Some` ⇒ THAT request failed
        // (the conn keeps serving — retryable); `req_id: None` ⇒
        // connection-fatal (terminal).
        FromSource::Error {
            req_id: Some(req_id),
            message,
        } if req_id == want_req => Err(PeerError::RequestFailed(message)),
        FromSource::Error {
            req_id: None,
            message,
        } => Err(PeerError::Server(message)),
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
        self.report_if(msg, || true);
    }

    fn report_if(&self, msg: HandlerControl, publish: impl FnOnce() -> bool) {
        let mut backlog = self.backlog.lock().expect("backlog poisoned");
        if !publish() {
            return;
        }
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

    /// Check publication after waiting for the backlog lock. The caller can
    /// serialize cancellation here without holding its lock during socket I/O.
    #[cfg(target_os = "linux")]
    pub(crate) fn report_if(&self, msg: HandlerControl, publish: impl FnOnce() -> bool) {
        self.inner.report_if(msg, publish);
    }

    /// Test-only: install a hook fired between a subscriber's backlog
    /// snapshot and its registration (while the backlog lock is held).
    #[cfg(test)]
    fn set_replay_hook(&self, cb: Box<dyn Fn() + Send>) {
        *self.inner.on_replay_done.lock().expect("hook poisoned") = Some(cb);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::TcpListener;

    use super::*;

    const CHUNK: u64 = 4096;
    const TOTAL: u64 = 4 * 4096;

    /// A full Linux accept queue makes the next TCP handshake stay pending.
    /// No routing assumptions or external black-hole address are needed.
    #[cfg(target_os = "linux")]
    #[test]
    fn cancellation_during_pending_connect_is_bounded() {
        let listener = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .unwrap();
        listener
            .bind(
                &"127.0.0.1:0"
                    .parse::<std::net::SocketAddr>()
                    .unwrap()
                    .into(),
            )
            .unwrap();
        listener.listen(0).unwrap();
        let addr = listener.local_addr().unwrap().as_socket().unwrap();
        // Linux permits backlog + 1 established connections. Never accept it.
        let _queued = TcpStream::connect_timeout(&addr, Duration::from_secs(1)).unwrap();
        let stopped = std::sync::Arc::new(AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = dial_with_socket(
                &addr.to_string(),
                "e",
                "tok",
                CHUNK,
                TOTAL,
                engram_migrate_proto::ConnPurpose::Fault,
                |socket| {
                    assert!(
                        socket.is_none(),
                        "full accept queue must prevent connection"
                    );
                    let _ = entered_tx.send(());
                    Ok(!worker_stopped.load(Ordering::SeqCst))
                },
            );
            done_tx.send(result).unwrap();
        });
        // The two checks occur before resolution and immediately before connect.
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        let started = std::time::Instant::now();
        stopped.store(true, Ordering::SeqCst);
        let result = done_rx
            .recv_timeout(PEER_READ_TIMEOUT + Duration::from_secs(1))
            .expect("canceled connect exceeded its deadline");
        assert!(
            matches!(result, Err(PeerError::Lost(ref message)) if message == "connection canceled")
        );
        assert!(started.elapsed() < PEER_READ_TIMEOUT + Duration::from_secs(1));
        worker.join().unwrap();
    }

    #[test]
    fn canceled_connect_does_not_resolve_or_open_a_socket() {
        let result = dial_with_socket(
            "invalid address",
            "e",
            "tok",
            CHUNK,
            TOTAL,
            engram_migrate_proto::ConnPurpose::Fault,
            |_| Ok(false),
        );
        assert!(
            matches!(result, Err(PeerError::Lost(ref message)) if message == "connection canceled")
        );
    }

    /// A minimal fake source: accepts conns, answers Hello/Ack/Seal,
    /// then serves canned NeedAt responses.
    pub(crate) fn fake_source(
        seal_bits: Vec<u64>,
        respond: impl Fn(u64, u64) -> FromSource + Send + Clone + 'static,
    ) -> std::net::SocketAddr {
        fake_source_geometry(seal_bits, CHUNK, TOTAL, respond)
    }

    pub(crate) fn fake_source_geometry(
        seal_bits: Vec<u64>,
        chunk_size: u64,
        total_bytes: u64,
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
                            chunk_size,
                            total_bytes,
                        },
                    )
                    .unwrap();
                    let mut bitmap = SealBitmap::new(chunk_size, total_bytes / chunk_size);
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
                            ToSource::NeedWindow {
                                req_id,
                                chunk_offsets,
                            } => {
                                for offset in chunk_offsets {
                                    let resp = respond(req_id, offset);
                                    let failed = matches!(resp, FromSource::Error { .. });
                                    if write_frame(&mut s, &resp).is_err() {
                                        return;
                                    }
                                    if failed {
                                        break;
                                    }
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

    pub(crate) fn page_resp(req_id: u64, chunk_offset: u64) -> FromSource {
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
    fn every_page_shape_checks_request_and_offset_before_install() {
        for response in [
            page_resp(7, CHUNK),
            FromSource::ZeroChunk {
                req_id: 7,
                chunk_offset: CHUNK,
            },
            FromSource::AltSource {
                req_id: 7,
                chunk_offset: CHUNK,
                durable_sha256: [0; 32],
            },
        ] {
            assert!(matches!(
                decode_page(response.clone(), 7, 0),
                Err(PeerError::Server(_))
            ));
            assert!(matches!(
                decode_page(response, 8, CHUNK),
                Err(PeerError::Server(_))
            ));
        }
        assert!(matches!(
            decode_page(
                FromSource::Error {
                    req_id: Some(8),
                    message: "wrong request".into(),
                },
                7,
                0
            ),
            Err(PeerError::Server(_))
        ));
    }

    #[test]
    fn reordered_mixed_window_fails_before_any_install() {
        let addr = fake_source(vec![0, 1], |req_id, offset| {
            if offset == 0 {
                FromSource::ZeroChunk {
                    req_id,
                    chunk_offset: CHUNK,
                }
            } else {
                FromSource::AltSource {
                    req_id,
                    chunk_offset: 0,
                    durable_sha256: [0; 32],
                }
            }
        });
        let peer =
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK, TOTAL).unwrap();
        let mut installs = 0;
        let result = peer.need_window(&[0, CHUNK], |_, _| {
            installs += 1;
            Ok::<_, ()>(())
        });
        assert!(matches!(result, Err(PeerError::Server(_))));
        assert_eq!(installs, 0);
        assert!(peer.is_lost());
    }

    #[test]
    fn duplicate_from_previous_window_fails_before_any_new_install() {
        let saved = std::sync::Arc::new(Mutex::new(None));
        let addr = fake_source(vec![0, 1], move |req_id, offset| {
            saved
                .lock()
                .unwrap()
                .get_or_insert_with(|| FromSource::ZeroChunk {
                    req_id,
                    chunk_offset: offset,
                })
                .clone()
        });
        let peer =
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK, TOTAL).unwrap();
        peer.need_at(0).unwrap();
        let mut installs = 0;
        let result = peer.need_window(&[CHUNK], |_, _| {
            installs += 1;
            Ok::<_, ()>(())
        });
        assert!(matches!(result, Err(PeerError::Server(_))));
        assert_eq!(installs, 0);
        assert!(peer.is_lost());
    }

    #[test]
    fn duplicate_reply_fails_before_installing_duplicate() {
        let addr = fake_source(vec![0, 1], |req_id, _| FromSource::ZeroChunk {
            req_id,
            chunk_offset: 0,
        });
        let peer =
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK, TOTAL).unwrap();
        let mut installed = Vec::new();
        let result = peer.need_window(&[0, CHUNK], |offset, _| {
            installed.push(offset);
            Ok::<_, ()>(())
        });
        assert!(matches!(result, Err(PeerError::Server(_))));
        // The valid prefix stays installed; the duplicate never reaches the installer.
        assert_eq!(installed, [0]);
        assert!(peer.is_lost());
    }

    #[test]
    fn window_streams_verified_pages_and_keeps_prefix_on_corruption() {
        let addr = fake_source(vec![0, 1, 2], |req_id, offset| {
            if offset == 2 * CHUNK {
                FromSource::Page {
                    req_id,
                    chunk_offset: offset,
                    bytes: vec![7],
                    hash: [0; 32],
                    lz4: false,
                }
            } else {
                page_resp(req_id, offset)
            }
        });
        let sess =
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK, TOTAL).unwrap();
        let mut installed = Vec::new();
        let err = sess
            .need_window(&[0, CHUNK, 2 * CHUNK], |offset, page| {
                let PeerPage::Bytes(bytes) = page else {
                    panic!("expected bytes")
                };
                assert_eq!(bytes, vec![0xAB; CHUNK as usize]);
                installed.push(offset);
                Ok::<_, ()>(())
            })
            .unwrap_err();
        assert!(matches!(err, PeerError::ShaMismatch { .. }));
        assert_eq!(installed, vec![0, CHUNK]);
        assert!(sess.is_lost());
        assert_eq!(sess.fault_stats().0, 1);
    }

    #[test]
    fn window_installs_before_next_response_and_retries_only_suffix() {
        let (tx, rx) = std::sync::mpsc::channel();
        let rx = std::sync::Arc::new(Mutex::new(rx));
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let seen_source = seen.clone();
        let failed = std::sync::Arc::new(AtomicBool::new(false));
        let addr = fake_source(vec![0, 1, 2], move |req, offset| {
            seen_source.lock().unwrap().push(offset);
            if offset == CHUNK && !failed.swap(true, Ordering::SeqCst) {
                // The server will not send chunk 1 until chunk 0 was installed.
                rx.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
                return FromSource::Error {
                    req_id: Some(req),
                    message: "transient read failure".into(),
                };
            }
            page_resp(req, offset)
        });
        let sess =
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK, TOTAL).unwrap();
        let mut installed = Vec::new();
        sess.need_window(&[0, CHUNK, 2 * CHUNK], |offset, _| {
            installed.push(offset);
            if offset == 0 {
                tx.send(()).unwrap();
            }
            Ok::<_, ()>(())
        })
        .unwrap()
        .unwrap();
        assert_eq!(installed, vec![0, CHUNK, 2 * CHUNK]);
        assert_eq!(*seen.lock().unwrap(), vec![0, CHUNK, CHUNK, 2 * CHUNK]);
        assert_eq!(sess.fault_stats().0, 1);
        assert!(!sess.is_lost());
    }

    #[test]
    fn one_chunk_window_matches_need_at() {
        let addr = fake_source(vec![0], page_resp);
        let sess =
            PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK, TOTAL).unwrap();
        let PeerPage::Bytes(single) =
            request_chunk(&mut sess.fault_conn.lock().unwrap(), &sess.next_req, 0).unwrap()
        else {
            panic!("bytes")
        };
        sess.need_window(&[0], |offset, page| {
            assert_eq!(offset, 0);
            let PeerPage::Bytes(bytes) = page else {
                panic!("bytes")
            };
            assert_eq!(bytes, single);
            Ok::<_, ()>(())
        })
        .unwrap()
        .unwrap();
    }

    #[test]
    fn window_skips_installed_and_unsealed_and_region_tail() {
        let mut seal = SealBitmap::new(4096, 8);
        for i in [0, 2, 3, 4, 5, 6, 7] {
            seal.set(i);
        }
        assert_eq!(
            fault_window(&seal, 0, 5 * 4096 + 100, 8, |i| i == 2),
            vec![0, 3 * 4096, 4 * 4096]
        );
        assert_eq!(fault_window(&seal, 0, 8 * 4096, 1, |_| false), vec![0]);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct("proptest-regressions/fault_window.txt"))),
            ..proptest::test_runner::Config::default()
        })]
        #[test]
        fn selected_window_is_sealed_uninstalled_and_bounded(
            bits in proptest::collection::vec((proptest::bool::ANY, proptest::bool::ANY), 1..128),
            first in 0usize..128, end in 1usize..129, limit in 1usize..257,
        ) {
            let end = end.min(bits.len());
            let first = first.min(end - 1);
            let mut seal = SealBitmap::new(4096, bits.len() as u64);
            for (i, &(sealed, _)) in bits.iter().enumerate() { if sealed { seal.set(i as u64); } }
            let got = fault_window(&seal, first as u64 * 4096, end as u64 * 4096, limit, |i| bits[i].1);
            let expected: Vec<_> = (first..end).filter(|&i| bits[i].0 && !bits[i].1).take(limit).map(|i| i as u64 * 4096).collect();
            proptest::prop_assert_eq!(got, expected);
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

    /// Regression for issue #227 (b): a per-request server error
    /// (`Error { req_id: Some }`) means "THAT request failed" per the wire
    /// contract — the connection keeps serving. `need_at` must classify it
    /// as `RequestFailed` and RETRY (not `mark_lost` + terminal), so one
    /// transient `process_vm_readv` EAGAIN/ENOMEM on the source never
    /// rewinds the whole migration.
    ///
    /// Pre-fix `decode_page` collapsed every `Error` into `PeerError::Server`
    /// and `need_at` mapped `Server(_)` unconditionally to `mark_lost()` +
    /// terminal, so this would have returned `Err` with the session latched
    /// lost.
    #[test]
    fn need_at_retries_per_request_error_then_succeeds() {
        let calls = std::sync::Arc::new(AtomicU64::new(0));
        let calls_for_src = std::sync::Arc::clone(&calls);
        let addr = fake_source(vec![0], move |req_id, chunk_offset| {
            // First NeedAt ever → per-request failure (req_id: Some);
            // every later one → a real page. Mirrors the source's
            // transient-readv reply that keeps the conn alive.
            if calls_for_src.fetch_add(1, Ordering::SeqCst) == 0 {
                FromSource::Error {
                    req_id: Some(req_id),
                    message: "transient readv EAGAIN".into(),
                }
            } else {
                page_resp(req_id, chunk_offset)
            }
        });
        let sess = PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK, TOTAL)
            .expect("connect");
        match sess
            .need_at(0)
            .expect("retried per-request error must succeed")
        {
            PeerPage::Bytes(b) => assert!(b.iter().all(|x| *x == 0xAB)),
            other => panic!("expected Bytes after retry, got {other:?}"),
        }
        // The peer must NOT be latched lost: the migration is unaffected.
        assert!(
            !sess.is_lost(),
            "per-request error must not mark the peer lost"
        );
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "expected a retry round-trip"
        );
    }

    /// Regression for issue #227 (b), terminal half: a connection-fatal
    /// server error (`Error { req_id: None }`) IS terminal — it latches the
    /// peer lost and never retries. This is the discriminant that must stay
    /// fatal so a genuinely-broken conn still surfaces PeerLost promptly.
    #[test]
    fn need_at_connection_fatal_error_is_terminal() {
        let addr = fake_source(vec![0], |_req_id, _chunk_offset| FromSource::Error {
            req_id: None,
            message: "unknown export".into(),
        });
        let sess = PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK, TOTAL)
            .expect("connect");
        let err = sess
            .need_at(0)
            .expect_err("connection-fatal error must be terminal");
        assert!(matches!(err, PeerError::Server(_)), "{err}");
        assert!(
            sess.is_lost(),
            "connection-fatal error must latch the peer lost"
        );
    }

    /// `decode_page` honors the wire contract directly: `Some` ⇒
    /// `RequestFailed` (retryable), `None` ⇒ `Server` (terminal).
    #[test]
    fn decode_page_distinguishes_request_scoped_from_fatal() {
        let req_scoped = decode_page(
            FromSource::Error {
                req_id: Some(7),
                message: "readv".into(),
            },
            7,
            0,
        );
        assert!(
            matches!(req_scoped, Err(PeerError::RequestFailed(_))),
            "req_id: Some must decode as RequestFailed, got {req_scoped:?}"
        );
        let fatal = decode_page(
            FromSource::Error {
                req_id: None,
                message: "bad hello".into(),
            },
            7,
            0,
        );
        assert!(
            matches!(fatal, Err(PeerError::Server(_))),
            "req_id: None must decode as Server, got {fatal:?}"
        );
    }

    /// Regression for issue #226 (a): every dialed page-channel conn
    /// must carry read/write deadlines + keepalive so a silently-dead
    /// source can't wedge a blocking `read_frame` forever. Pre-fix
    /// `dial` set only `set_nodelay`, leaving `read_timeout`/
    /// `write_timeout` as `None`.
    #[test]
    fn dialed_fault_conn_has_read_write_timeouts() {
        let addr = fake_source(vec![0], page_resp);
        let sess = PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK, TOTAL)
            .expect("connect");
        let (read_to, write_to) = sess.fault_conn_timeouts();
        assert_eq!(
            read_to,
            Some(PEER_READ_TIMEOUT),
            "fault conn must carry a read deadline (#226)"
        );
        assert_eq!(
            write_to,
            Some(PEER_WRITE_TIMEOUT),
            "fault conn must carry a write deadline (#226)"
        );
    }

    /// Regression for issue #226 (a), the failure it actually prevents:
    /// a source that completes the handshake then goes SILENT (no FIN/
    /// RST — power loss / partition) must NOT wedge `need_at` forever.
    /// The read timeout fires, classifies `Io` (retryable), the bounded
    /// redial loop runs, and the session latches `Lost` — surfacing
    /// `PeerLost` to the caller instead of a permanently-parked vCPU.
    ///
    /// Pre-fix (`read_frame`'s `read_exact` on a conn with no read
    /// timeout) this test would hang indefinitely rather than returning.
    #[test]
    fn silent_source_after_handshake_surfaces_lost_not_hang() {
        // A source that handshakes (Hello/Ack/Seal) on every connection
        // but NEVER answers a NeedAt — and holds the socket open so
        // there is no EOF/RST to fail on. This is the no-unacked-data
        // black hole the issue describes; only a read timeout can break
        // it. Each redial gets the same silent treatment.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let Ok(ToSource::Hello { .. }) = read_frame::<_, ToSource>(&mut s) else {
                    continue;
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
                // Go silent: never read the NeedAt, never reply, never
                // close. Keep the stream alive so the dest sees no FIN.
                held.push(s);
            }
        });

        let sess = PeerSession::connect(addr.to_string(), "e".into(), "tok".into(), CHUNK, TOTAL)
            .expect("connect");
        let start = std::time::Instant::now();
        let err = sess
            .need_at(0)
            .expect_err("a silent source must surface an error, not hang");
        let elapsed = start.elapsed();
        assert!(
            matches!(err, PeerError::Lost(_)),
            "silent source must latch Lost, got {err}"
        );
        assert!(sess.is_lost(), "session must be latched lost");
        // It must have RETURNED (the whole point) and within the bounded
        // read-timeout * redial budget — generously capped to absorb CI
        // scheduling jitter while still proving it isn't an unbounded
        // hang.
        assert!(
            elapsed < Duration::from_secs(60),
            "need_at took {elapsed:?} — read timeout/redial budget not bounding the wait"
        );
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
                    while let Ok(ToSource::NeedWindow {
                        req_id,
                        chunk_offsets,
                    }) = read_frame::<_, ToSource>(&mut s)
                    {
                        for offset in chunk_offsets {
                            if write_frame(&mut s, &page_resp(req_id, offset)).is_err() {
                                return;
                            }
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
                        fault_around_chunks_installed: 0,
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
                    fault_around_chunks_installed: 0,
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
