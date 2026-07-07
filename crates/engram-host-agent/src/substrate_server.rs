//! ADR 0075: the substrate populate server — the WRITER side of the
//! single-writer chunk-cache discipline.
//!
//! Binds `<work_dir>/substrate.sock` and serves the per-VM uffd
//! handlers (read-only clients): `Hello` answers readiness from live
//! probes (never config echoes), `Populate` runs the pin → get →
//! open → send-fd → unpin sequence against the host-agent's ONE
//! `ChunkCache` instance — so the global singleflight, the pin set,
//! verify-on-populate, and the #528 budget sweep all apply to handler
//! traffic exactly as they do to the host-agent's own.
//!
//! The clients are synchronous blocking threads (the fault path), so
//! the protocol is one-request-one-response per frame — no pipelining,
//! no backpressure subtleties. Each accepted connection gets a tokio
//! task that does blocking UDS I/O via `spawn_blocking`-free direct
//! reads on a dedicated std thread: simplest correct shape, and the
//! populate work itself (cache.get) is async on the server side.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use engram_chunk_store::{ChunkCache, ChunkHash, ChunkStore};
use engram_substrate_proto::{FromWriter, ToWriter, PROTO_VERSION};

/// Everything a connection needs to answer requests.
#[derive(Clone)]
pub struct SubstrateServer {
    cache: ChunkCache,
    store: Arc<ChunkStore>,
    /// The uffd base-shm dir; `HelloAck.tmpfs_ok` is a live statfs of
    /// this path (TMPFS_MAGIC), closing the handler-side half of the
    /// post-roll "register memory … userfaultfd … System error" window.
    uffd_base_dir: PathBuf,
    /// Tokio runtime handle for the async cache/store calls, entered
    /// from the per-connection std threads.
    rt: tokio::runtime::Handle,
}

impl SubstrateServer {
    pub fn new(cache: ChunkCache, store: Arc<ChunkStore>, uffd_base_dir: PathBuf) -> Self {
        Self {
            cache,
            store,
            uffd_base_dir,
            rt: tokio::runtime::Handle::current(),
        }
    }

    /// Bind and serve. Replaces any stale socket file (a rolled
    /// host-agent's successor re-binds the same path — the clients'
    /// bounded reconnect covers the gap; their LAST-RESORT direct-blob
    /// fallback covers a wedged writer).
    pub fn spawn(self, sock_path: PathBuf) -> std::io::Result<std::thread::JoinHandle<()>> {
        let _ = std::fs::remove_file(&sock_path);
        if let Some(parent) = sock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&sock_path)?;
        tracing::info!(sock = %sock_path.display(), "substrate populate server bound");
        Ok(std::thread::Builder::new()
            .name("substrate-accept".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    match conn {
                        Ok(stream) => {
                            let server = self.clone();
                            if let Err(e) = std::thread::Builder::new()
                                .name("substrate-conn".into())
                                .spawn(move || server.serve_conn(stream))
                            {
                                tracing::warn!(error = %e, "substrate conn thread spawn failed");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "substrate accept failed");
                        }
                    }
                }
            })
            .expect("spawn substrate accept thread"))
    }

    fn serve_conn(&self, mut stream: UnixStream) {
        // ADR 0075 phase 2: chunks pinned for this connection's
        // lifetime (the session manifest from Hello). Unpinned on every
        // exit path below.
        let mut conn_pins: Vec<ChunkHash> = Vec::new();
        let result = self.serve_conn_inner(&mut stream, &mut conn_pins);
        for h in conn_pins {
            self.cache.unpin(h);
        }
        if let Err(e) = result {
            tracing::debug!(error = %e, "substrate connection ended");
        }
    }

    fn serve_conn_inner(
        &self,
        stream: &mut UnixStream,
        conn_pins: &mut Vec<ChunkHash>,
    ) -> std::io::Result<()> {
        loop {
            let msg: ToWriter = match engram_substrate_proto::read_frame(stream) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e),
            };
            let res = match msg {
                ToWriter::Hello {
                    proto_version,
                    canonical_manifest,
                    session_manifest,
                } => {
                    if proto_version != PROTO_VERSION {
                        engram_substrate_proto::write_frame(
                            stream,
                            &FromWriter::PopulateErr {
                                msg: format!(
                                    "proto version mismatch: writer {PROTO_VERSION}, client {proto_version}"
                                ),
                            },
                        )?;
                        return Ok(());
                    }
                    // `canonical_manifest` is accepted for the ADR 0076
                    // owner (whose readiness registry can answer
                    // staged-ness); the host-agent answers the two live
                    // probes it CAN answer honestly.
                    let _ = canonical_manifest;
                    // ADR 0075 phase 2: pin the session manifest's
                    // divergent chunks for this connection's lifetime,
                    // so the single evictor can never unlink them out
                    // from under the handler's faults. A manifest the
                    // store can't resolve (a migration destination's
                    // deliberately-unpublished v+1) skips pinning —
                    // those chunks are resident + peer-served.
                    if conn_pins.is_empty() {
                        if let Some(mref) = session_manifest {
                            match self.rt.block_on(self.store.get_manifest(mref)) {
                                Ok(manifest) => {
                                    for chunk in &manifest.chunks {
                                        self.cache.pin(chunk.hash);
                                        conn_pins.push(chunk.hash);
                                    }
                                    tracing::debug!(
                                        pinned = conn_pins.len(),
                                        "substrate: pinned session chunk set for connection lifetime",
                                    );
                                }
                                Err(e) => {
                                    tracing::debug!(error = %e, "session manifest unavailable; no pins");
                                }
                            }
                        }
                    }
                    let ack = FromWriter::HelloAck {
                        tmpfs_ok: dir_is_tmpfs(&self.uffd_base_dir),
                        cache_writable: self.cache.root_writable_probe(),
                    };
                    engram_substrate_proto::write_frame(stream, &ack)
                }
                ToWriter::Populate { hash } => self.populate(stream, ChunkHash::from_bytes(hash)),
            };
            res?;
        }
    }

    /// The pin-around-open sequence (ADR 0075): the pin holds the
    /// single evictor off between populate and open; the fd makes a
    /// post-reply unlink harmless.
    fn populate(&self, stream: &mut UnixStream, hash: ChunkHash) -> std::io::Result<()> {
        self.cache.pin(hash);
        let already = self.cache.contains_on_disk(hash);
        let fetched = self.rt.block_on(async {
            let store = self.store.clone();
            self.cache
                .get(hash, || async move { store.get_chunk(hash).await })
                .await
        });
        let outcome = match fetched {
            Ok(_bytes) => {
                let path = self.cache.on_disk_path(hash);
                match std::fs::File::open(&path) {
                    Ok(file) => {
                        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
                        engram_substrate_proto::write_frame(
                            stream,
                            &FromWriter::Populated { len },
                        )?;
                        #[cfg(target_os = "linux")]
                        {
                            use std::os::fd::AsFd;
                            engram_substrate_proto::send_fd(stream, file.as_fd())?;
                        }
                        #[cfg(not(target_os = "linux"))]
                        {
                            // Non-Linux never has out-of-process clients
                            // (ADR 0075) — this server is only exercised
                            // by tests on macOS, which read the cache
                            // dir directly after Populated.
                            let _ = file;
                        }
                        if already {
                            "already_local"
                        } else {
                            "populated"
                        }
                    }
                    Err(e) => {
                        engram_substrate_proto::write_frame(
                            stream,
                            &FromWriter::PopulateErr {
                                msg: format!("open after populate: {e}"),
                            },
                        )?;
                        "error"
                    }
                }
            }
            Err(e) => {
                engram_substrate_proto::write_frame(
                    stream,
                    &FromWriter::PopulateErr {
                        msg: format!("populate fetch: {e}"),
                    },
                )?;
                "error"
            }
        };
        self.cache.unpin(hash);
        ::metrics::counter!(
            crate::metrics::SUBSTRATE_POPULATE_REQUESTS_TOTAL,
            "outcome" => outcome
        )
        .increment(1);
        Ok(())
    }
}

/// Live probe: is `dir` on a tmpfs? (The uffd base-shm contract —
/// ADR 0045 — requires it; a handler serving off non-tmpfs later dies
/// with `register memory … userfaultfd … System error`.)
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

#[cfg(not(target_os = "linux"))]
fn dir_is_tmpfs(_dir: &Path) -> bool {
    // macOS has no tmpfs contract (VZ restores don't use the uffd
    // substrate); answer true so dev flows aren't blocked by a probe
    // for a Linux-only invariant.
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::traits::BlobStorage;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Counting blob store: serves fixed bytes, counts fetches — the
    /// cross-process-singleflight assertion's witness.
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

    fn server_fixture(dir: &std::path::Path) -> (SubstrateServer, Arc<CountingBlob>, ChunkHash) {
        let blob = Arc::new(CountingBlob {
            inner: engram_storage_local::LocalBlobStorage::new(dir.join("blobs")),
            gets: AtomicU64::new(0),
        });
        let cache = ChunkCache::new(engram_chunk_store::cache::ChunkCacheConfig::new(
            dir.join("chunk-cache"),
        ));
        let store = Arc::new(
            ChunkStore::new(blob.clone() as Arc<dyn BlobStorage>).with_chunk_cache(cache.clone()),
        );
        let bytes = bytes::Bytes::from(vec![7u8; 1024]);
        let hash = engram_chunk_store::ChunkHash::of(&bytes);
        let store_for_put = store.clone();
        tokio::runtime::Handle::current()
            .block_on(async move { store_for_put.put_chunk(&bytes).await })
            .expect("seed chunk");
        // Reset the counter — the seed's write-through may have read.
        blob.gets.store(0, Ordering::SeqCst);
        let server = SubstrateServer::new(cache, store, dir.join("shm"));
        (server, blob, hash)
    }

    /// N concurrent populate requests for one hash across ≥2 client
    /// connections produce exactly ≤1 blob fetch — the writer's global
    /// singleflight collapses cross-client dupes (pre-0069 each
    /// handler process fetched independently).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_clients_collapse_to_one_fetch() {
        let dir = tempfile::tempdir().unwrap();
        // Evict the seeded chunk from local cache so populate must fetch.
        let (server, blob, hash) = tokio::task::block_in_place(|| server_fixture(dir.path()));
        server.cache.evict_on_disk_for_test(hash);

        let sock = dir.path().join("substrate.sock");
        let _accept = server.clone().spawn(sock.clone()).unwrap();

        let mut joins = Vec::new();
        for _ in 0..4 {
            let sock = sock.clone();
            joins.push(std::thread::spawn(move || {
                let mut stream = UnixStream::connect(&sock).expect("connect");
                engram_substrate_proto::write_frame(
                    &mut stream,
                    &engram_substrate_proto::ToWriter::Populate {
                        hash: *hash.as_bytes(),
                    },
                )
                .unwrap();
                let reply: engram_substrate_proto::FromWriter =
                    engram_substrate_proto::read_frame(&mut stream).unwrap();
                matches!(reply, engram_substrate_proto::FromWriter::Populated { .. })
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
        // The chunk landed in the cache (write-through) — a reader sees it.
        assert!(server.cache.contains_on_disk(hash));
    }

    /// HelloAck answers live probes; the writability probe is
    /// cross-platform (tmpfs is Linux-contract territory).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hello_answers_probes() {
        let dir = tempfile::tempdir().unwrap();
        let (server, _blob, _hash) = tokio::task::block_in_place(|| server_fixture(dir.path()));
        let sock = dir.path().join("substrate.sock");
        let _accept = server.spawn(sock.clone()).unwrap();

        let ack = std::thread::spawn(move || {
            let mut stream = UnixStream::connect(&sock).expect("connect");
            engram_substrate_proto::write_frame(
                &mut stream,
                &engram_substrate_proto::ToWriter::Hello {
                    proto_version: engram_substrate_proto::PROTO_VERSION,
                    canonical_manifest: None,
                    session_manifest: None,
                },
            )
            .unwrap();
            engram_substrate_proto::read_frame::<_, engram_substrate_proto::FromWriter>(&mut stream)
                .unwrap()
        })
        .join()
        .unwrap();
        match ack {
            engram_substrate_proto::FromWriter::HelloAck { cache_writable, .. } => {
                assert!(cache_writable, "tempdir cache root must probe writable");
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }
    }
}
