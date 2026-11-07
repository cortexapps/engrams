//! The substrate populate server: the writer side of the
//! single-writer chunk cache (ADR 0075).
//!
//! Binds `<work_dir>/substrate.sock` and serves the per-VM uffd
//! handlers, one tokio task per connection. `Hello` answers readiness
//! from live probes and pins the session manifest's chunks while any
//! connection names that manifest. `Populate` gets the chunk from the fill engine
//! at [`Class::Demand`] and delivers the bytes it got: protocol version
//! 2 copies them into the connection's shared-memory slot; version 1
//! passes an fd (see `engram-substrate-proto`).

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use bytes::Bytes;
use engram_chunk_store::{ChunkCache, ChunkHash};
use engram_core::types::manifest::ManifestRef;
use engram_fill::{Class, FillEngine};
use engram_substrate_proto::{
    ByteRange, FromWriter, Requester, ToWriter, PROTO_VERSION, PROTO_VERSION_READ,
    PROTO_VERSION_V1, SLOT_BYTES,
};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

/// Everything a connection needs to answer requests.
#[derive(Clone)]
pub struct SubstrateServer {
    cache: ChunkCache,
    fill: FillEngine,
    /// The uffd base-shm dir; `HelloAck.tmpfs_ok` is a live statfs of
    /// this path.
    uffd_base_dir: PathBuf,
    /// The pinned chunk set of each session manifest that a live
    /// connection names. All connections for one manifest share one
    /// entry, so the manifest is read and pinned once, not per dial.
    pins: Arc<Mutex<HashMap<ManifestRef, Weak<SessionPins>>>>,
}

/// Per-connection state.
struct Conn {
    /// The version the client's `Hello` asked for (1 without one).
    version: u32,
    /// The version-2 slot; `None` on a version-1 connection.
    slot: Option<std::fs::File>,
    /// The length of the last reply in the slot.
    slot_used: u64,
    /// The engine, scoped to the session manifest's hinted peer.
    fill: FillEngine,
    pins: Option<Arc<SessionPins>>,
}

/// A slot reply longer than this has its pages freed once the client is
/// done with it: a 512 KiB memory chunk stays, a 16 MiB disk chunk goes.
const SLOT_PUNCH_ABOVE: u64 = 1024 * 1024;

impl Conn {
    /// Free the pages of the last reply when it was long. Called when a
    /// frame arrives: the exchange is strictly request/response, and the
    /// client reads the slot only between a reply and its next request,
    /// so by then it is done with the bytes.
    fn release_slot(&mut self) {
        let used = std::mem::take(&mut self.slot_used);
        let Some(slot) = self.slot.as_ref().filter(|_| used > SLOT_PUNCH_ABOVE) else {
            return;
        };
        if let Err(e) = engram_host_core::disk::punch_hole(slot, 0, used) {
            tracing::debug!(error = %e, used, "substrate slot pages not freed");
        }
    }
}

/// A session manifest's chunks, pinned while a connection holds this
/// entry and unpinned when the last one drops it, so an ended or
/// cancelled connection never leaks a pin.
struct SessionPins {
    cache: ChunkCache,
    hashes: tokio::sync::OnceCell<Vec<ChunkHash>>,
}

impl Drop for SessionPins {
    fn drop(&mut self) {
        if let Some(hashes) = self.hashes.get() {
            self.cache.unpin_all(hashes.iter().copied());
        }
    }
}

impl SubstrateServer {
    pub fn new(cache: ChunkCache, fill: FillEngine, uffd_base_dir: PathBuf) -> Self {
        Self {
            cache,
            fill,
            uffd_base_dir,
            pins: Arc::default(),
        }
    }

    /// Bind and serve on the current tokio runtime. Replaces a stale
    /// socket file: a rolled host-agent's successor binds the same
    /// path, and the handlers redial.
    pub fn spawn(self, sock_path: PathBuf) -> std::io::Result<tokio::task::JoinHandle<()>> {
        let _ = std::fs::remove_file(&sock_path);
        if let Some(parent) = sock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&sock_path)?;
        tracing::info!(sock = %sock_path.display(), "substrate populate server bound");
        Ok(tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let server = self.clone();
                        tokio::spawn(async move { server.serve_conn(stream).await });
                    }
                    Err(e) => {
                        // EMFILE and similar persist; do not spin on them.
                        tracing::warn!(error = %e, "substrate accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }))
    }

    async fn serve_conn(&self, mut stream: UnixStream) {
        let mut conn = Conn {
            version: PROTO_VERSION_V1,
            slot: None,
            slot_used: 0,
            fill: self.fill.clone(),
            pins: None,
        };
        if let Err(e) = self.serve_frames(&mut stream, &mut conn).await {
            tracing::debug!(error = %e, "substrate connection ended");
        }
    }

    async fn serve_frames(&self, stream: &mut UnixStream, conn: &mut Conn) -> std::io::Result<()> {
        while let Some(msg) = read_frame(stream).await? {
            conn.release_slot();
            match msg {
                ToWriter::Hello {
                    proto_version,
                    canonical_manifest: _,
                    session_manifest,
                } => {
                    // The slot exists before the ack, so a failure to
                    // create it is a `PopulateErr` and the handler dials
                    // again with version 1.
                    let slot = match proto_version {
                        PROTO_VERSION | PROTO_VERSION_READ => match new_slot() {
                            Ok(slot) => Some(slot),
                            Err(e) => {
                                let msg = format!("create slot: {e}");
                                write_frame(stream, &FromWriter::PopulateErr { msg }).await?;
                                return Ok(());
                            }
                        },
                        PROTO_VERSION_V1 => None,
                        other => {
                            let msg = format!(
                                "proto version mismatch: writer {PROTO_VERSION_V1}..={PROTO_VERSION_READ}, client {other}"
                            );
                            write_frame(stream, &FromWriter::PopulateErr { msg }).await?;
                            return Ok(());
                        }
                    };
                    if let Some(mref) = session_manifest {
                        conn.fill = self.fill.for_manifest(mref.manifest_id);
                        if conn.pins.is_none() {
                            conn.pins = Some(self.pin_session_chunks(mref).await);
                        }
                    }
                    write_frame(stream, &self.hello_ack().await?).await?;
                    if let Some(slot) = &slot {
                        send_fd(stream, slot.as_fd()).await?;
                    }
                    conn.slot = slot;
                    conn.version = proto_version;
                }
                ToWriter::Populate { hash } => {
                    self.populate(stream, conn, ChunkHash::from_bytes(hash))
                        .await?
                }
                ToWriter::Read {
                    hash,
                    requester,
                    range,
                } if conn.version >= PROTO_VERSION_READ => {
                    self.read(stream, conn, ChunkHash::from_bytes(hash), requester, range)
                        .await?
                }
                ToWriter::Read { .. } => {
                    let msg = format!(
                        "read needs protocol version {PROTO_VERSION_READ}; this connection speaks {}",
                        conn.version
                    );
                    write_frame(stream, &FromWriter::PopulateErr { msg }).await?;
                }
            }
        }
        Ok(())
    }

    /// Pin the session manifest's chunks so the evictor never removes
    /// them while the handler can fault them. The first connection for
    /// a manifest reads it and pins; the others wait for that read and
    /// share its pins. A manifest the store cannot resolve (a migration
    /// destination's unpublished manifest) pins nothing, and the next
    /// connection reads it again; its chunks are resident or come from
    /// the peer.
    async fn pin_session_chunks(&self, mref: ManifestRef) -> Arc<SessionPins> {
        let pins = {
            let mut map = self.pins.lock();
            match map.get(&mref).and_then(Weak::upgrade) {
                Some(pins) => pins,
                None => {
                    map.retain(|_, pins| pins.strong_count() > 0);
                    let pins = Arc::new(SessionPins {
                        cache: self.cache.clone(),
                        hashes: tokio::sync::OnceCell::new(),
                    });
                    map.insert(mref, Arc::downgrade(&pins));
                    pins
                }
            }
        };
        let pinned = pins
            .hashes
            .get_or_try_init(|| async {
                let manifest = self.fill.store().get_manifest(mref).await?;
                let hashes: Vec<ChunkHash> = manifest.chunks.iter().map(|c| c.hash).collect();
                self.cache.pin_all(hashes.iter().copied());
                tracing::debug!(
                    pinned = hashes.len(),
                    "substrate: pinned session chunk set while a connection names it",
                );
                Ok::<_, engram_chunk_store::ChunkStoreError>(hashes)
            })
            .await;
        if let Err(e) = pinned {
            tracing::debug!(error = %e, "session manifest unavailable; no pins");
        }
        pins
    }

    async fn hello_ack(&self) -> std::io::Result<FromWriter> {
        let dir = self.uffd_base_dir.clone();
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || FromWriter::HelloAck {
            tmpfs_ok: dir_is_tmpfs(&dir),
            cache_writable: cache.root_writable_probe(),
        })
        .await
        .map_err(std::io::Error::other)
    }

    async fn populate(
        &self,
        stream: &mut UnixStream,
        conn: &mut Conn,
        hash: ChunkHash,
    ) -> std::io::Result<()> {
        let already = self.cache.contains_on_disk(hash);
        let delivered = match conn.fill.get(hash, Class::Demand).await {
            Ok(bytes) => self.deliver(stream, conn, hash, &bytes).await?,
            Err(e) => Err(format!("populate fetch: {e}")),
        };
        let outcome = match delivered {
            Ok(()) if already => "already_local",
            Ok(()) => "populated",
            Err(msg) => {
                write_frame(stream, &FromWriter::PopulateErr { msg }).await?;
                "error"
            }
        };
        ::metrics::counter!(
            crate::metrics::SUBSTRATE_POPULATE_REQUESTS_TOTAL,
            "outcome" => outcome,
            "proto" => if conn.slot.is_some() { "2" } else { "1" },
        )
        .increment(1);
        Ok(())
    }

    /// Serve a version-3 read at `Demand`, through the connection's engine
    /// view (its peer, and the sandbox's `Demand` cap when one is
    /// registered for the session manifest the `Hello` named).
    async fn read(
        &self,
        stream: &mut UnixStream,
        conn: &mut Conn,
        hash: ChunkHash,
        requester: Requester,
        range: Option<ByteRange>,
    ) -> std::io::Result<()> {
        let got = match (range, requester) {
            (Some(range), _) => match usize::try_from(range.len) {
                Ok(len) => conn
                    .fill
                    .read_range(hash, range.offset, len, Class::Demand)
                    .await
                    .map_err(|e| format!("read range: {e}")),
                Err(_) => Err(format!("range of {} bytes is too long", range.len)),
            },
            (None, Requester::Memory) => conn
                .fill
                .get(hash, Class::Demand)
                .await
                .map_err(|e| format!("read: {e}")),
            (None, Requester::Disk) => conn
                .fill
                .read(hash, Class::Demand)
                .await
                .map_err(|e| format!("read: {e}")),
        };
        let delivered = match got {
            Ok(bytes) => self.deliver(stream, conn, hash, &bytes).await?,
            Err(msg) => Err(msg),
        };
        let outcome = match delivered {
            Ok(()) => "read",
            Err(msg) => {
                write_frame(stream, &FromWriter::PopulateErr { msg }).await?;
                "error"
            }
        };
        ::metrics::counter!(
            crate::metrics::SUBSTRATE_POPULATE_REQUESTS_TOTAL,
            "outcome" => outcome,
            "proto" => "3",
        )
        .increment(1);
        Ok(())
    }

    /// Send `bytes` in the connection's protocol version. The outer
    /// error is a broken connection; the inner one is a reply the
    /// handler gets as `PopulateErr`.
    async fn deliver(
        &self,
        stream: &mut UnixStream,
        conn: &mut Conn,
        hash: ChunkHash,
        bytes: &Bytes,
    ) -> std::io::Result<Result<(), String>> {
        let len = bytes.len() as u64;
        if let Some(slot) = &conn.slot {
            if len > SLOT_BYTES {
                return Ok(Err(format!(
                    "chunk of {len} bytes exceeds the {SLOT_BYTES}-byte slot"
                )));
            }
            // A copy into shared memory, not disk I/O.
            let written = slot.write_all_at(bytes, 0);
            conn.slot_used = len;
            if let Err(e) = written {
                return Ok(Err(format!("write slot: {e}")));
            }
            write_frame(stream, &FromWriter::Filled { len }).await?;
            return Ok(Ok(()));
        }
        // Version 1. The cache file holds the same bytes when it is
        // there; when it is not (evicted, or its write is still queued),
        // an anonymous file carries the bytes in hand.
        let file = match std::fs::File::open(self.cache.on_disk_path(hash)) {
            Ok(file) => file,
            Err(_) => match anon_file_with(bytes) {
                Ok(file) => file,
                Err(e) => return Ok(Err(format!("anonymous chunk file: {e}"))),
            },
        };
        write_frame(stream, &FromWriter::Populated { len }).await?;
        send_fd(stream, file.as_fd()).await?;
        Ok(Ok(()))
    }
}

/// One frame, or `None` at a clean end of stream.
async fn read_frame<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
) -> std::io::Result<Option<T>> {
    let mut header = [0u8; 4];
    match stream.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut body = vec![0u8; engram_substrate_proto::frame_body_len(header)?];
    stream.read_exact(&mut body).await?;
    engram_substrate_proto::decode_frame_body(&body).map(Some)
}

async fn write_frame<T: serde::Serialize>(stream: &mut UnixStream, msg: &T) -> std::io::Result<()> {
    stream
        .write_all(&engram_substrate_proto::encode_frame(msg)?)
        .await
}

async fn send_fd(stream: &UnixStream, fd: BorrowedFd<'_>) -> std::io::Result<()> {
    stream
        .async_io(tokio::io::Interest::WRITABLE, || {
            engram_substrate_proto::send_fd(stream.as_fd(), fd)
        })
        .await
}

/// A sparse anonymous file of [`SLOT_BYTES`].
fn new_slot() -> std::io::Result<std::fs::File> {
    let slot = anon_file()?;
    slot.set_len(SLOT_BYTES)?;
    Ok(slot)
}

/// An anonymous file that holds `bytes` from offset 0, with its file
/// offset at 0.
fn anon_file_with(bytes: &[u8]) -> std::io::Result<std::fs::File> {
    let file = anon_file()?;
    file.write_all_at(bytes, 0)?;
    Ok(file)
}

#[cfg(target_os = "linux")]
fn anon_file() -> std::io::Result<std::fs::File> {
    let fd =
        nix::sys::memfd::memfd_create(c"engram-substrate", nix::sys::memfd::MFdFlags::MFD_CLOEXEC)?;
    Ok(std::fs::File::from(fd))
}

/// No memfd off Linux; an unlinked temp file behaves the same for the
/// tests that run the server there.
#[cfg(not(target_os = "linux"))]
fn anon_file() -> std::io::Result<std::fs::File> {
    tempfile::tempfile()
}

/// Live probe: is `dir` on a tmpfs? The uffd base shm (ADR 0045) must
/// be; a handler that serves off another file system later dies with
/// `register memory … userfaultfd … System error`.
#[cfg(target_os = "linux")]
fn dir_is_tmpfs(dir: &Path) -> bool {
    match nix::sys::statfs::statfs(dir) {
        Ok(fs) => fs.filesystem_type() == nix::sys::statfs::TMPFS_MAGIC,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "tmpfs probe failed");
            false
        }
    }
}

/// macOS has no tmpfs contract (VZ restores do not use the uffd
/// substrate).
#[cfg(not(target_os = "linux"))]
fn dir_is_tmpfs(_dir: &Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_chunk_store::ChunkStore;
    use engram_core::traits::BlobStorage;
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// Counting blob store: serves fixed bytes, counts fetches.
    struct CountingBlob {
        inner: engram_storage_local::LocalBlobStorage,
        gets: AtomicU64,
    }

    #[async_trait::async_trait]
    impl BlobStorage for CountingBlob {
        async fn put_streaming(
            &self,
            key: &str,
            body: engram_core::ByteStream,
        ) -> Result<u64, engram_core::BlobError> {
            self.inner.put_streaming(key, body).await
        }
        async fn get_streaming(
            &self,
            key: &str,
        ) -> Result<engram_core::ByteStream, engram_core::BlobError> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            self.inner.get_streaming(key).await
        }
        async fn head(
            &self,
            key: &str,
        ) -> Result<engram_core::BlobObjectMeta, engram_core::BlobError> {
            self.inner.head(key).await
        }
        async fn delete(&self, key: &str) -> Result<(), engram_core::BlobError> {
            self.inner.delete(key).await
        }
        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, engram_core::BlobError> {
            self.inner.list_prefix(prefix).await
        }
    }

    async fn server_fixture(
        dir: &std::path::Path,
    ) -> (SubstrateServer, Arc<CountingBlob>, ChunkHash, bytes::Bytes) {
        let blob = Arc::new(CountingBlob {
            inner: engram_storage_local::LocalBlobStorage::new(dir.join("blobs")),
            gets: AtomicU64::new(0),
        });
        let cache = ChunkCache::new(engram_chunk_store::cache::ChunkCacheConfig::new(
            dir.join("chunk-cache"),
        ));
        let store =
            ChunkStore::new(blob.clone() as Arc<dyn BlobStorage>).with_chunk_cache(cache.clone());
        let bytes = bytes::Bytes::from((0..64 * 1024).map(|i| i as u8).collect::<Vec<_>>());
        let hash = store.put_chunk(&bytes).await.expect("seed chunk");
        blob.gets.store(0, Ordering::SeqCst);
        let fill = FillEngine::new(store, engram_fill::Config::default());
        let server = SubstrateServer::new(cache, fill, dir.join("shm"));
        (server, blob, hash, bytes)
    }

    fn hello(stream: &mut StdUnixStream, proto_version: u32) -> FromWriter {
        engram_substrate_proto::write_frame(
            stream,
            &ToWriter::Hello {
                proto_version,
                canonical_manifest: None,
                session_manifest: None,
            },
        )
        .unwrap();
        engram_substrate_proto::read_frame(stream).unwrap()
    }

    fn populate(stream: &mut StdUnixStream, hash: ChunkHash) -> FromWriter {
        engram_substrate_proto::write_frame(
            stream,
            &ToWriter::Populate {
                hash: *hash.as_bytes(),
            },
        )
        .unwrap();
        engram_substrate_proto::read_frame(stream).unwrap()
    }

    /// Concurrent populates for one hash on several connections make at
    /// most one blob fetch: the writer's singleflight collapses them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_clients_collapse_to_one_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let (server, blob, hash, _) = server_fixture(dir.path()).await;
        server.cache.evict_on_disk_for_test(hash);

        let sock = dir.path().join("substrate.sock");
        let _accept = server.clone().spawn(sock.clone()).unwrap();

        let mut joins = Vec::new();
        for _ in 0..4 {
            let sock = sock.clone();
            joins.push(std::thread::spawn(move || {
                let mut stream = StdUnixStream::connect(&sock).expect("connect");
                matches!(populate(&mut stream, hash), FromWriter::Populated { .. })
            }));
        }
        for j in joins {
            assert!(j.join().unwrap(), "populate must succeed");
        }
        let fetches = blob.gets.load(Ordering::SeqCst);
        assert!(
            fetches <= 1,
            "singleflight must collapse concurrent misses to one fetch, saw {fetches}",
        );
        assert!(server.cache.contains_on_disk(hash));
    }

    /// HelloAck answers live probes; the writability probe is
    /// cross-platform (tmpfs is a Linux contract).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hello_answers_probes() {
        let dir = tempfile::tempdir().unwrap();
        let (server, _blob, _hash, _) = server_fixture(dir.path()).await;
        let sock = dir.path().join("substrate.sock");
        let _accept = server.spawn(sock.clone()).unwrap();

        let ack = tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&sock).expect("connect");
            hello(&mut stream, PROTO_VERSION_V1)
        })
        .await
        .unwrap();
        match ack {
            FromWriter::HelloAck { cache_writable, .. } => {
                assert!(cache_writable, "tempdir cache root must probe writable");
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }
    }

    /// Version 2: the ack carries the slot fd, and each `Filled` reply
    /// puts the chunk in the slot, also after the cache file is gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn version_two_delivers_chunks_through_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let (server, _blob, hash, bytes) = server_fixture(dir.path()).await;
        let cache = server.cache.clone();
        let sock = dir.path().join("substrate.sock");
        let _accept = server.spawn(sock.clone()).unwrap();

        tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&sock).expect("connect");
            assert!(matches!(
                hello(&mut stream, PROTO_VERSION),
                FromWriter::HelloAck { .. }
            ));
            let slot = std::fs::File::from(
                engram_substrate_proto::recv_fd(stream.as_fd()).expect("slot fd"),
            );
            assert_eq!(slot.metadata().unwrap().len(), SLOT_BYTES);
            for _ in 0..2 {
                let FromWriter::Filled { len } = populate(&mut stream, hash) else {
                    panic!("expected Filled");
                };
                let mut got = vec![0u8; len as usize];
                slot.read_exact_at(&mut got, 0).unwrap();
                assert_eq!(got, bytes.as_ref());
                cache.evict_on_disk_for_test(hash);
            }
        })
        .await
        .unwrap();
    }

    /// A version-1 handler gets an fd that reads the whole chunk from
    /// offset 0, from the cache file or, when the file is gone at
    /// delivery, from an anonymous file.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn version_one_fd_reads_the_whole_chunk() {
        use std::io::Read as _;
        let dir = tempfile::tempdir().unwrap();
        let (server, _blob, hash, bytes) = server_fixture(dir.path()).await;
        let sock = dir.path().join("substrate.sock");
        let _accept = server.spawn(sock.clone()).unwrap();

        let expected = bytes.clone();
        tokio::task::spawn_blocking(move || {
            let bytes = expected;
            let mut stream = StdUnixStream::connect(&sock).expect("connect");
            assert!(matches!(
                hello(&mut stream, PROTO_VERSION_V1),
                FromWriter::HelloAck { .. }
            ));
            let FromWriter::Populated { len } = populate(&mut stream, hash) else {
                panic!("expected Populated");
            };
            let mut file = std::fs::File::from(
                engram_substrate_proto::recv_fd(stream.as_fd()).expect("chunk fd"),
            );
            let mut got = Vec::new();
            file.read_to_end(&mut got).unwrap();
            assert_eq!(got.len() as u64, len);
            assert_eq!(got, bytes.as_ref());
        })
        .await
        .unwrap();

        let anon = anon_file_with(&bytes).unwrap();
        let mut got = Vec::new();
        (&anon).read_to_end(&mut got).unwrap();
        assert_eq!(got, bytes.as_ref());
    }

    /// Connections that name one session manifest read it once and share
    /// one pin per chunk; the pins go when the last connection closes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connections_for_one_manifest_read_and_pin_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let (server, blob, _hash, _) = server_fixture(dir.path()).await;
        let store = server.fill.store().clone();
        let src = dir.path().join("mem.bin");
        // Four distinct chunks.
        let body: Vec<u8> = (0..4 * 4096u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        std::fs::write(&src, body).unwrap();
        let manifest = store
            .chunk_file(
                &src,
                engram_chunk_store::manifest::ManifestKind::Memory,
                Some(4096),
            )
            .await
            .unwrap();
        let mref = ManifestRef::new();
        // Another process wrote the manifest, so the server reads it.
        ChunkStore::new(blob.clone() as Arc<dyn BlobStorage>)
            .put_manifest(mref, &manifest)
            .await
            .unwrap();
        blob.gets.store(0, Ordering::SeqCst);
        let cache = server.cache.clone();
        let sock = dir.path().join("substrate.sock");
        let _accept = server.spawn(sock.clone()).unwrap();

        let streams = tokio::task::spawn_blocking(move || {
            (0..3)
                .map(|_| {
                    let mut stream = StdUnixStream::connect(&sock).expect("connect");
                    engram_substrate_proto::write_frame(
                        &mut stream,
                        &ToWriter::Hello {
                            proto_version: PROTO_VERSION_V1,
                            canonical_manifest: None,
                            session_manifest: Some(mref),
                        },
                    )
                    .unwrap();
                    let ack: FromWriter = engram_substrate_proto::read_frame(&mut stream).unwrap();
                    assert!(matches!(ack, FromWriter::HelloAck { .. }));
                    stream
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap();
        assert_eq!(blob.gets.load(Ordering::SeqCst), 1, "one manifest read");
        let chunks: Vec<ChunkHash> = manifest.chunks.iter().map(|c| c.hash).collect();
        assert!(chunks.iter().all(|h| cache.is_pinned(*h)));
        for h in &chunks {
            cache.unpin(*h);
        }
        assert!(
            chunks.iter().all(|h| !cache.is_pinned(*h)),
            "each chunk holds one pin, not one per connection"
        );
        for h in &chunks {
            cache.pin(*h);
        }

        drop(streams);
        // At most 5 s of 10 ms polls.
        for poll in 0.. {
            if !chunks.iter().any(|h| cache.is_pinned(*h)) {
                break;
            }
            assert!(poll < 500, "pins outlive the connections");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn read(stream: &mut StdUnixStream, hash: ChunkHash, range: Option<ByteRange>) -> FromWriter {
        engram_substrate_proto::write_frame(
            stream,
            &ToWriter::Read {
                hash: *hash.as_bytes(),
                requester: Requester::Disk,
                range,
            },
        )
        .unwrap();
        engram_substrate_proto::read_frame(stream).unwrap()
    }

    /// Version 3: a disk read of a range lands the range in the slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn version_three_reads_a_range_through_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let (server, _blob, hash, bytes) = server_fixture(dir.path()).await;
        let sock = dir.path().join("substrate.sock");
        let _task = server.spawn(sock.clone()).unwrap();
        let want = bytes.slice(4096..8192);
        tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&sock).unwrap();
            assert!(matches!(
                hello(&mut stream, PROTO_VERSION_READ),
                FromWriter::HelloAck { .. }
            ));
            let slot =
                std::fs::File::from(engram_substrate_proto::recv_fd(stream.as_fd()).unwrap());
            let range = ByteRange {
                offset: 4096,
                len: 4096,
            };
            assert_eq!(
                read(&mut stream, hash, Some(range)),
                FromWriter::Filled { len: 4096 }
            );
            let mut got = vec![0u8; 4096];
            slot.read_exact_at(&mut got, 0).unwrap();
            assert_eq!(got, want.as_ref());
        })
        .await
        .unwrap();
    }

    /// The pages of a long reply are freed when the next request comes.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_next_request_frees_a_long_reply() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let (server, _blob, small, _) = server_fixture(dir.path()).await;
        let long = bytes::Bytes::from(vec![7u8; 4 * 1024 * 1024]);
        let long = server.fill.store().put_chunk(&long).await.unwrap();
        let sock = dir.path().join("substrate.sock");
        let _task = server.spawn(sock.clone()).unwrap();
        tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&sock).unwrap();
            assert!(matches!(
                hello(&mut stream, PROTO_VERSION_READ),
                FromWriter::HelloAck { .. }
            ));
            let slot =
                std::fs::File::from(engram_substrate_proto::recv_fd(stream.as_fd()).unwrap());
            let held = |slot: &std::fs::File| slot.metadata().unwrap().blocks() * 512;
            assert!(matches!(
                populate(&mut stream, long),
                FromWriter::Filled { .. }
            ));
            assert!(held(&slot) >= 4 * 1024 * 1024);
            assert!(matches!(
                populate(&mut stream, small),
                FromWriter::Filled { .. }
            ));
            assert!(held(&slot) < 1024 * 1024, "held {}", held(&slot));
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_read_on_a_version_two_connection_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (server, _blob, hash, _) = server_fixture(dir.path()).await;
        let sock = dir.path().join("substrate.sock");
        let _task = server.spawn(sock.clone()).unwrap();
        tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&sock).unwrap();
            assert!(matches!(
                hello(&mut stream, PROTO_VERSION),
                FromWriter::HelloAck { .. }
            ));
            let _slot = engram_substrate_proto::recv_fd(stream.as_fd()).unwrap();
            assert!(matches!(
                read(&mut stream, hash, None),
                FromWriter::PopulateErr { .. }
            ));
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (server, _blob, _hash, _) = server_fixture(dir.path()).await;
        let sock = dir.path().join("substrate.sock");
        let _accept = server.spawn(sock.clone()).unwrap();

        // A writer older than version 3 answers a version-3 Hello the same
        // way: an error, then the end of the connection.
        let (reply, after) = tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&sock).expect("connect");
            let reply = hello(&mut stream, PROTO_VERSION_READ + 1);
            let after = engram_substrate_proto::read_frame::<_, FromWriter>(&mut stream);
            (reply, after)
        })
        .await
        .unwrap();
        assert!(matches!(reply, FromWriter::PopulateErr { .. }));
        assert_eq!(after.unwrap_err().kind(), std::io::ErrorKind::UnexpectedEof);
    }
}
