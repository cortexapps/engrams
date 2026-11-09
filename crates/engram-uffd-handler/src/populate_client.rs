//! The read-only client of the substrate populate socket (ADR 0075).
//!
//! The handler asks the one writer (the host-agent) for each chunk the
//! cache does not hold. Synchronous: it runs on the fault threads.
//! Handlers outlive host-agent rolls (ADR 0044 K2), so a request that
//! finds the socket gone redials a bounded number of times; past that
//! the caller fetches the chunk from the object store itself.
//!
//! Each connection speaks protocol version 2 (the chunk arrives in the
//! connection's shared-memory slot) or, against a writer that refuses
//! version 2, version 1 (the chunk arrives as an fd). After a refusal the
//! client dials version 1 until a connection breaks (the writer rolled);
//! the next dial asks for version 2 again. #1772 removes version 1.
//!
//! [`PopulateClient::hello`] fills the pool with one connection per fault
//! thread, so the first faults do not wait for a dial.

use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use engram_chunk_store::ChunkHash;
use engram_core::types::manifest::ManifestRef;
use engram_substrate_proto::{FromWriter, ToWriter, PROTO_VERSION, PROTO_VERSION_V1, SLOT_BYTES};

use crate::mapped::{chunk_file_bytes, Mapping};

/// Dial attempts per request. A rolled host-agent's successor binds
/// the socket within seconds; a dead writer must surface fast so the
/// caller's object-store fallback keeps the vCPU moving.
const RECONNECT_ATTEMPTS: u32 = 3;
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);
/// A cold chunk costs the writer an object-store fetch (about 190 ms
/// mean). A writer that does not answer in this time is wedged: the
/// request goes to the fallback with no retry.
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum PopulateError {
    /// No answer from the writer within the dial or reply budget; the
    /// caller should fetch from the object store.
    WriterUnreachable(String),
    /// The writer answered but could not deliver the chunk.
    Failed(String),
}

impl std::fmt::Display for PopulateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WriterUnreachable(m) => write!(f, "substrate writer unreachable: {m}"),
            Self::Failed(m) => write!(f, "populate failed: {m}"),
        }
    }
}

impl std::error::Error for PopulateError {}

/// The `Hello` manifests: canonical, then session.
type HelloManifests = (Option<ManifestRef>, Option<ManifestRef>);

/// `HelloAck` probe results: `(tmpfs_ok, cache_writable)`.
pub type HelloProbes = (bool, bool);

/// Synchronous populate client, safe to call from every fault thread at
/// once (the writer's singleflight collapses duplicate chunks).
pub struct PopulateClient {
    sock_path: PathBuf,
    /// Connections not in use. A request takes one or dials a new one,
    /// so concurrent fault threads each get their own exchange.
    idle: Mutex<Vec<Conn>>,
    /// The manifests from [`Self::hello`], sent again on every dial: the
    /// writer keeps the session's chunks pinned while a connection names
    /// them, and a rolled writer starts with no pins.
    hello_manifests: Mutex<Option<HelloManifests>>,
    /// The writer refused version 2; dial version 1 directly.
    writer_v1: AtomicBool,
}

struct Conn {
    stream: UnixStream,
    /// The writer's slot, mapped read-only; `None` on version 1.
    slot: Option<Mapping>,
}

enum Reply {
    /// The chunk is in the connection's slot, `len` bytes.
    Slot(usize),
    Bytes(Bytes),
    Failed(String),
}

enum HelloReply {
    Ack(HelloProbes),
    Refused(String),
}

impl Conn {
    fn exchange(&mut self, hash: ChunkHash) -> std::io::Result<Reply> {
        engram_substrate_proto::write_frame(
            &mut self.stream,
            &ToWriter::Populate {
                hash: *hash.as_bytes(),
            },
        )?;
        match engram_substrate_proto::read_frame(&mut self.stream)? {
            FromWriter::Filled { len } if self.slot.is_some() && len <= SLOT_BYTES => {
                Ok(Reply::Slot(len as usize))
            }
            FromWriter::Populated { len } => {
                let file = File::from(engram_substrate_proto::recv_fd(self.stream.as_fd())?);
                let bytes = chunk_file_bytes(&file)?;
                if bytes.len() as u64 != len {
                    return Err(invalid(format!(
                        "populate fd holds {} bytes, reply said {len}",
                        bytes.len()
                    )));
                }
                Ok(Reply::Bytes(bytes))
            }
            FromWriter::PopulateErr { msg } => Ok(Reply::Failed(msg)),
            other => Err(invalid(format!("unexpected Populate reply: {other:?}"))),
        }
    }
}

/// A chunk in a connection's slot. It holds the connection out of the
/// pool until it is dropped, so no request rewrites the slot while
/// these bytes are in use.
struct SlotLease {
    /// `None` only inside `drop`.
    conn: Option<Conn>,
    len: usize,
    client: Arc<PopulateClient>,
}

impl AsRef<[u8]> for SlotLease {
    fn as_ref(&self) -> &[u8] {
        match self.conn.as_ref().and_then(|c| c.slot.as_ref()) {
            // SAFETY: `len <= SLOT_BYTES`, the mapped length and the
            // slot file's size (checked at dial). The writer writes the
            // slot only while it serves a request on this connection,
            // and the lease holds the connection, so none is in flight.
            Some(slot) => unsafe { slot.prefix(self.len) },
            None => &[],
        }
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.client.checkin(conn);
        }
    }
}

impl PopulateClient {
    pub fn new(sock_path: PathBuf) -> Self {
        Self {
            sock_path,
            idle: Mutex::new(Vec::new()),
            hello_manifests: Mutex::new(None),
            writer_v1: AtomicBool::new(false),
        }
    }

    /// Startup handshake. Returns the probe results so `main` can refuse
    /// to serve on an unready host, before it binds the FC-facing UDS.
    /// After the first connection it dials `pool - 1` more for the pool;
    /// a failed extra dial leaves the pool smaller and a request dials
    /// later. The manifests are sent again on every later dial.
    pub fn hello(
        &self,
        canonical_manifest: Option<ManifestRef>,
        session_manifest: Option<ManifestRef>,
        pool: usize,
    ) -> Result<HelloProbes, PopulateError> {
        let manifests = (canonical_manifest, session_manifest);
        *self.hello_manifests.lock().expect("hello_manifests lock") = Some(manifests);
        self.idle.lock().expect("populate idle lock").clear();
        self.writer_v1.store(false, Ordering::Relaxed);
        let mut last_err = None;
        for _ in 0..RECONNECT_ATTEMPTS {
            match self.dial_with_hello(manifests) {
                Ok((conn, probes)) => {
                    self.checkin(conn);
                    for _ in 1..pool {
                        match self.dial_with_hello(manifests) {
                            Ok((conn, _)) => self.checkin(conn),
                            Err(e) => {
                                tracing::debug!(error = %e, "populate pool dial failed");
                                break;
                            }
                        }
                    }
                    return Ok(probes);
                }
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(RECONNECT_BACKOFF);
                }
            }
        }
        Err(unreachable_err(last_err))
    }

    /// The bytes of one chunk. On version 2 they are the slot itself,
    /// and the connection returns to the pool when the last clone of
    /// the `Bytes` drops; on version 1 they map the fd the writer
    /// passed (an eviction unlink after the reply is harmless).
    pub fn request(self: &Arc<Self>, hash: ChunkHash) -> Result<Bytes, PopulateError> {
        let mut last_err = None;
        for _ in 0..RECONNECT_ATTEMPTS {
            let pooled = self.idle.lock().expect("populate idle lock").pop();
            let mut conn = match pooled.map_or_else(|| self.dial(), Ok) {
                Ok(conn) => conn,
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(RECONNECT_BACKOFF);
                    continue;
                }
            };
            match conn.exchange(hash) {
                Ok(Reply::Slot(len)) => {
                    return Ok(Bytes::from_owner(SlotLease {
                        conn: Some(conn),
                        len,
                        client: Arc::clone(self),
                    }))
                }
                Ok(Reply::Bytes(bytes)) => {
                    self.checkin(conn);
                    return Ok(bytes);
                }
                Ok(Reply::Failed(msg)) => {
                    self.checkin(conn);
                    return Err(PopulateError::Failed(msg));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(PopulateError::WriterUnreachable(format!(
                        "no reply within {READ_TIMEOUT:?}: {e}"
                    )));
                }
                Err(e) => {
                    // A broken connection means the writer went away;
                    // the idle ones are as stale. Redial now.
                    tracing::debug!(error = %e, "populate exchange failed; redialing");
                    self.idle.lock().expect("populate idle lock").clear();
                    self.writer_v1.store(false, Ordering::Relaxed);
                    last_err = Some(e);
                }
            }
        }
        Err(unreachable_err(last_err))
    }

    fn checkin(&self, conn: Conn) {
        self.idle.lock().expect("populate idle lock").push(conn);
    }

    fn connect(&self) -> std::io::Result<UnixStream> {
        let s = UnixStream::connect(&self.sock_path)?;
        s.set_read_timeout(Some(READ_TIMEOUT))?;
        s.set_write_timeout(Some(WRITE_TIMEOUT))?;
        Ok(s)
    }

    /// A new connection. Before [`Self::hello`] it sends no `Hello` and
    /// speaks version 1.
    fn dial(&self) -> std::io::Result<Conn> {
        let manifests = *self.hello_manifests.lock().expect("hello_manifests lock");
        match manifests {
            Some(manifests) => self.dial_with_hello(manifests).map(|(conn, _)| conn),
            None => Ok(Conn {
                stream: self.connect()?,
                slot: None,
            }),
        }
    }

    /// A new connection that asks for version 2 and, if the writer
    /// refuses, speaks version 1 on a second connection. After a refusal
    /// it asks for version 1 at once.
    fn dial_with_hello(&self, manifests: HelloManifests) -> std::io::Result<(Conn, HelloProbes)> {
        if self.writer_v1.load(Ordering::Relaxed) {
            return self.dial_v1(manifests);
        }
        let mut stream = self.connect()?;
        match hello_on(&mut stream, PROTO_VERSION, manifests)? {
            HelloReply::Ack(probes) => {
                let slot = File::from(engram_substrate_proto::recv_fd(stream.as_fd())?);
                if slot.metadata()?.len() != SLOT_BYTES {
                    return Err(invalid("slot file is not SLOT_BYTES long".into()));
                }
                let slot = Mapping::lazy(slot.as_fd(), SLOT_BYTES as usize)?;
                let conn = Conn {
                    stream,
                    slot: Some(slot),
                };
                Ok((conn, probes))
            }
            HelloReply::Refused(msg) => {
                tracing::debug!(%msg, "writer refused protocol v2; using v1");
                self.writer_v1.store(true, Ordering::Relaxed);
                self.dial_v1(manifests)
            }
        }
    }

    fn dial_v1(&self, manifests: HelloManifests) -> std::io::Result<(Conn, HelloProbes)> {
        let mut stream = self.connect()?;
        match hello_on(&mut stream, PROTO_VERSION_V1, manifests)? {
            HelloReply::Ack(probes) => Ok((Conn { stream, slot: None }, probes)),
            HelloReply::Refused(msg) => Err(invalid(msg)),
        }
    }
}

fn hello_on(
    stream: &mut UnixStream,
    proto_version: u32,
    (canonical_manifest, session_manifest): HelloManifests,
) -> std::io::Result<HelloReply> {
    engram_substrate_proto::write_frame(
        stream,
        &ToWriter::Hello {
            proto_version,
            canonical_manifest,
            session_manifest,
        },
    )?;
    match engram_substrate_proto::read_frame(stream)? {
        FromWriter::HelloAck {
            tmpfs_ok,
            cache_writable,
        } => Ok(HelloReply::Ack((tmpfs_ok, cache_writable))),
        FromWriter::PopulateErr { msg } => Ok(HelloReply::Refused(msg)),
        other => Err(invalid(format!("unexpected Hello reply: {other:?}"))),
    }
}

fn invalid(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

fn unreachable_err(last: Option<std::io::Error>) -> PopulateError {
    PopulateError::WriterUnreachable(last.map_or_else(|| "no attempts".into(), |e| e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A writer that refuses version 2 the way a version-1 writer does
    /// (`PopulateErr`, then close), then takes `v1_conns` connections
    /// that each must open with a version-1 `Hello`, and serves on them.
    fn v1_writer(listener: UnixListener, chunk: Vec<u8>, v1_conns: usize) {
        let (mut refused, _) = listener.accept().unwrap();
        let hello: ToWriter = engram_substrate_proto::read_frame(&mut refused).unwrap();
        assert!(matches!(
            hello,
            ToWriter::Hello {
                proto_version: PROTO_VERSION,
                ..
            }
        ));
        engram_substrate_proto::write_frame(
            &mut refused,
            &FromWriter::PopulateErr {
                msg: "proto version mismatch: writer 1, client 2".into(),
            },
        )
        .unwrap();
        drop(refused);

        let served: Vec<_> = (0..v1_conns)
            .map(|_| {
                let (s, _) = listener.accept().unwrap();
                let chunk = chunk.clone();
                std::thread::spawn(move || serve_v1(s, &chunk))
            })
            .collect();
        for s in served {
            s.join().unwrap();
        }
    }

    /// One version-1 connection: the `Hello`, then populates until the
    /// client closes it.
    fn serve_v1(mut s: UnixStream, chunk: &[u8]) {
        use std::io::Write as _;
        let hello: ToWriter = engram_substrate_proto::read_frame(&mut s).unwrap();
        assert!(matches!(
            hello,
            ToWriter::Hello {
                proto_version: PROTO_VERSION_V1,
                ..
            }
        ));
        engram_substrate_proto::write_frame(
            &mut s,
            &FromWriter::HelloAck {
                tmpfs_ok: true,
                cache_writable: true,
            },
        )
        .unwrap();
        while let Ok(_populate) = engram_substrate_proto::read_frame::<_, ToWriter>(&mut s) {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(chunk).unwrap();
            engram_substrate_proto::write_frame(
                &mut s,
                &FromWriter::Populated {
                    len: chunk.len() as u64,
                },
            )
            .unwrap();
            engram_substrate_proto::send_fd(s.as_fd(), file.as_fd()).unwrap();
        }
    }

    /// Against a version-1 writer, the pool's later dials ask for
    /// version 1 at once: one refused connection in all, not one per dial.
    #[test]
    fn falls_back_to_version_one_against_an_older_writer() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("substrate.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let chunk: Vec<u8> = (0..8192u32).map(|i| (i * 3) as u8).collect();
        let writer = std::thread::spawn({
            let chunk = chunk.clone();
            move || v1_writer(listener, chunk, 3)
        });
        let client = Arc::new(PopulateClient::new(sock));
        assert_eq!(client.hello(None, None, 3).unwrap(), (true, true));
        assert_eq!(client.idle.lock().unwrap().len(), 3);
        let got = client.request(ChunkHash::of(&chunk)).unwrap();
        assert_eq!(got.as_ref(), chunk.as_slice());
        drop(got);
        drop(client);
        writer.join().unwrap();
    }

    #[test]
    fn a_dead_socket_is_unreachable_within_the_dial_budget() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(PopulateClient::new(dir.path().join("absent.sock")));
        let started = std::time::Instant::now();
        let err = client.request(ChunkHash::of(b"x")).unwrap_err();
        assert!(matches!(err, PopulateError::WriterUnreachable(_)));
        assert!(started.elapsed() < RECONNECT_BACKOFF * (RECONNECT_ATTEMPTS + 1));
    }
}
