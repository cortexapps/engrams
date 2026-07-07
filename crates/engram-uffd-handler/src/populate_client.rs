//! ADR 0075: the read-only client of the substrate populate socket.
//!
//! The handler no longer owns a `ChunkCache` — misses are resolved by
//! asking the one writer (the host-agent) to populate and hand back an
//! `O_RDONLY` fd. Fully synchronous (the fault loop is a blocking
//! thread; the `peer.rs` no-tokio discipline applies), with bounded
//! reconnect: handlers deliberately OUTLIVE host-agent rolls (ADR 0044
//! K2), so a `request` landing while the successor re-binds the socket
//! retries briefly — and the CALLER falls back to a direct blob fetch
//! (served from memory, never written to the cache dir) when the
//! writer stays unreachable, so a fault can never wedge on the control
//! plane being mid-roll.

#[cfg(target_os = "linux")]
use std::io::Read as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use engram_chunk_store::ChunkHash;
use engram_substrate_proto::{FromWriter, ToWriter, PROTO_VERSION};

/// Bounded reconnect: a rolling host-agent's successor re-binds the
/// socket within seconds; a dead writer must surface fast so the
/// caller's direct-blob fallback keeps the vCPU un-wedged.
const RECONNECT_ATTEMPTS: u32 = 3;
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);
/// The writer's populate includes a GCS fetch on a cold chunk
/// (~190 ms mean, tail above that) — give it room without letting a
/// wedged writer hold a fault hostage (the fallback exists).
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum PopulateError {
    /// Writer unreachable past the retry budget — the caller should
    /// take the direct-blob fallback.
    WriterUnreachable(String),
    /// The writer answered but could not populate (blob-store error).
    Failed(String),
    Io(std::io::Error),
}

impl std::fmt::Display for PopulateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WriterUnreachable(m) => write!(f, "substrate writer unreachable: {m}"),
            Self::Failed(m) => write!(f, "populate failed: {m}"),
            Self::Io(e) => write!(f, "populate io: {e}"),
        }
    }
}

impl std::error::Error for PopulateError {}

/// Synchronous populate client. One connection, lazily (re)dialed
/// under a mutex — the fault loop serializes chunk misses per handler
/// anyway (the writer's singleflight collapses cross-handler dupes).
pub struct PopulateClient {
    sock_path: PathBuf,
    conn: Mutex<Option<UnixStream>>,
    /// The manifest refs from the startup `hello`, replayed on every
    /// fresh dial: the server-side session-chunk PIN SET (and the
    /// proto-version check) live for the CONNECTION, and the designed-
    /// for reconnect case — a host-agent roll, whose successor holds a
    /// brand-new empty pin set — is exactly when losing them silently
    /// would let the evictor thrash this handler's divergent chunks
    /// for the rest of the sandbox's life.
    hello_manifests: Mutex<
        Option<(
            Option<engram_core::types::manifest::ManifestRef>,
            Option<engram_core::types::manifest::ManifestRef>,
        )>,
    >,
    /// The most recent `HelloAck`'s probe results, written by the
    /// fresh-dial replay in `with_conn`.
    last_hello_ack: Mutex<Option<(bool, bool)>>,
}

impl PopulateClient {
    pub fn new(sock_path: PathBuf) -> Self {
        Self {
            sock_path,
            conn: Mutex::new(None),
            hello_manifests: Mutex::new(None),
            last_hello_ack: Mutex::new(None),
        }
    }

    fn dial(&self) -> std::io::Result<UnixStream> {
        let s = UnixStream::connect(&self.sock_path)?;
        s.set_read_timeout(Some(READ_TIMEOUT))?;
        s.set_write_timeout(Some(WRITE_TIMEOUT))?;
        Ok(s)
    }

    /// The `Hello` exchange on `stream`. Shared by the startup
    /// handshake and the re-dial replay.
    fn hello_on(
        stream: &mut UnixStream,
        canonical_manifest: Option<engram_core::types::manifest::ManifestRef>,
        session_manifest: Option<engram_core::types::manifest::ManifestRef>,
    ) -> std::io::Result<(bool, bool)> {
        engram_substrate_proto::write_frame(
            stream,
            &ToWriter::Hello {
                proto_version: PROTO_VERSION,
                canonical_manifest,
                session_manifest,
            },
        )?;
        let reply: FromWriter = engram_substrate_proto::read_frame(stream)?;
        match reply {
            FromWriter::HelloAck {
                tmpfs_ok,
                cache_writable,
            } => Ok((tmpfs_ok, cache_writable)),
            FromWriter::PopulateErr { msg } => {
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, msg))
            }
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unexpected Hello reply: {other:?}"),
            )),
        }
    }

    /// Startup handshake: send `Hello`, await `HelloAck`. Returns the
    /// probe results so `main` can refuse to serve on an unready host
    /// (exit BEFORE binding the FC-facing UDS — the ADR 0075 readiness
    /// ordering, same pattern as `peer.rs` connect-before-bind). The
    /// manifest refs are remembered and REPLAYED on every reconnect —
    /// the pins die with the connection, so a redial without a fresh
    /// `Hello` would leave the handler unpinned forever.
    pub fn hello(
        &self,
        canonical_manifest: Option<engram_core::types::manifest::ManifestRef>,
        session_manifest: Option<engram_core::types::manifest::ManifestRef>,
    ) -> Result<(bool, bool), PopulateError> {
        *self.hello_manifests.lock().expect("hello_manifests lock") =
            Some((canonical_manifest, session_manifest));
        // Force a fresh dial: with_conn runs the Hello exchange on
        // every new connection (the replay path) and caches the ack.
        *self.conn.lock().expect("populate conn lock") = None;
        self.with_conn(|_| Ok(()))?;
        self.last_hello_ack
            .lock()
            .expect("last_hello_ack lock")
            .ok_or_else(|| {
                PopulateError::WriterUnreachable("Hello exchange produced no ack".into())
            })
    }

    /// Populate one chunk and return its bytes (read from the fd the
    /// writer passed — an eviction unlink after the reply is harmless).
    pub fn request(&self, hash: ChunkHash) -> Result<Vec<u8>, PopulateError> {
        let bytes = self.with_conn(|stream| {
            engram_substrate_proto::write_frame(
                stream,
                &ToWriter::Populate {
                    hash: *hash.as_bytes(),
                },
            )?;
            let reply: FromWriter = engram_substrate_proto::read_frame(stream)?;
            match reply {
                FromWriter::Populated { len } => {
                    #[cfg(target_os = "linux")]
                    {
                        let fd = engram_substrate_proto::recv_fd(stream)?;
                        let mut file = std::fs::File::from(fd);
                        let mut buf = Vec::with_capacity(len as usize);
                        file.read_to_end(&mut buf)?;
                        Ok(Ok(buf))
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        // No SCM_RIGHTS off-Linux, and no out-of-process
                        // clients exist there (ADR 0075) — this arm is
                        // only reachable from unit tests, which use the
                        // fallback path.
                        let _ = len;
                        Err(std::io::Error::new(
                            std::io::ErrorKind::Unsupported,
                            "fd-passing populate is Linux-only",
                        ))
                    }
                }
                FromWriter::PopulateErr { msg } => Ok(Err(msg)),
                other => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unexpected Populate reply: {other:?}"),
                )),
            }
        })?;
        bytes.map_err(PopulateError::Failed)
    }

    /// Run `f` against the live connection, redialing (bounded) on
    /// connect/transport failure. A mid-exchange error poisons the
    /// connection (drop + redial next attempt) — the protocol is
    /// strictly request/response, so a fresh dial is always safe.
    fn with_conn<T>(
        &self,
        mut f: impl FnMut(&mut UnixStream) -> std::io::Result<T>,
    ) -> Result<T, PopulateError> {
        let mut guard = self.conn.lock().expect("populate conn lock");
        let mut last_err = None;
        for attempt in 0..RECONNECT_ATTEMPTS {
            if guard.is_none() {
                match self.dial() {
                    Ok(mut s) => {
                        // Replay `Hello` on EVERY fresh connection: the
                        // server's session-chunk pin set (and its proto
                        // check) are per-connection state, and the
                        // designed-for reconnect — a rolled host-agent's
                        // successor, holding a brand-new empty pin set —
                        // is exactly when silently skipping it would
                        // leave this handler unpinned for the rest of
                        // the sandbox's life.
                        let manifests = *self.hello_manifests.lock().expect("hello_manifests lock");
                        if let Some((c, sm)) = manifests {
                            match Self::hello_on(&mut s, c, sm) {
                                Ok(ack) => {
                                    *self.last_hello_ack.lock().expect("last_hello_ack lock") =
                                        Some(ack);
                                }
                                Err(e) => {
                                    last_err = Some(e);
                                    std::thread::sleep(RECONNECT_BACKOFF);
                                    continue;
                                }
                            }
                        }
                        *guard = Some(s);
                    }
                    Err(e) => {
                        last_err = Some(e);
                        std::thread::sleep(RECONNECT_BACKOFF);
                        continue;
                    }
                }
            }
            let stream = guard.as_mut().expect("conn present");
            match f(stream) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    tracing::debug!(attempt, error = %e, "populate exchange failed; redialing");
                    *guard = None;
                    last_err = Some(e);
                    std::thread::sleep(RECONNECT_BACKOFF);
                }
            }
        }
        Err(PopulateError::WriterUnreachable(
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no attempts".into()),
        ))
    }
}
