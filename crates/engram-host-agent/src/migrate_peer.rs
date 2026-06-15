//! ADR 0045 C2: the source-side page server — the host-agent end of the
//! post-copy page channel (`engram-migrate-proto` over TCP, default 9102).
//!
//! ONE listener per host, `export_id`-routed: R8 caps in-flight
//! migrations at one per source and one per dest, so port contention is
//! a non-problem and a single firewall/config surface wins. The server
//! is registered with [`PeerExport`]s by the C2 capture (which owns the
//! pause + pagemap scan); without exports every connection is rejected
//! at `Hello` — the listener is inert until a migration is in flight.
//!
//! Serving posture (the soundness story lives in `engram-migrate-proto`
//! docs and the ADR):
//!
//! - `NeedAt` reads the chunk from the PAUSED source FC's address space
//!   via `process_vm_readv` — legal because the host-agent is FC's
//!   parent (YAMA), and correct-by-construction for still-absent pages:
//!   the readv faults them through the source's OWN uffd handler
//!   (`pagemap_probe.c` T3). This is why the source FC *and* its handler
//!   must stay alive (vCPUs paused) until the dest's drain finishes.
//! - all-zero chunks answer `ZeroChunk` (ZEROPAGE-installed/zeroed-COW
//!   chunks classify dirty in the pagemap scan; shipping literal zeros
//!   would dominate an idle guest's drain);
//! - chunks whose live bytes hash to the durable manifest entry answer
//!   `AltSource` (the over-approximation demote);
//! - everything else ships as `Page` with its sha256.
//! - `GetChunk` serves allowlisted durable chunks from the local NVMe
//!   cache (the dest handler's mid-drain fallback when its own cache
//!   misses) — same allowlist posture as the gRPC `MigrationFetch`.
//!
//! Connections are handled on `spawn_blocking` threads with the proto's
//! sync codec (symmetric with the handler client; ≤ a few conns per
//! migration × one migration per host). Requests are served serially per
//! connection — `req_id` keeps the wire order-independent so the dest
//! scales by opening more drain connections, not by a fancier stream.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use engram_chunk_store::{ChunkCache, ChunkHash, ChunkStore};
use engram_core::SandboxId;
use engram_migrate_proto::{
    read_frame, write_frame, FromSource, SealBitmap, ToSource, PROTO_VERSION,
};
use sha2::{Digest, Sha256};

use crate::dirty_map::GuestVma;

/// Everything the server needs to serve one frozen sandbox's export.
/// Registered by the C2 capture after the pagemap scan; removed at
/// commit/abort (which also drops the capture guard and destroys or
/// un-pauses the VM — registry lifetime mirrors `MigrationRegistry`).
pub struct PeerExport {
    pub export_id: String,
    /// Per-export random secret; the dest handler presents it in `Hello`.
    /// Compared hash-to-hash (constant-time by construction).
    pub token: String,
    pub sandbox_id: SandboxId,
    /// The paused source FC process — the readv target.
    pub fc_pid: u32,
    /// Base-shm-backed VMAs of `fc_pid`, sorted by `file_offset`.
    pub vmas: Vec<GuestVma>,
    /// The post-pause dirty map; `NeedAt` is only legal for sealed chunks.
    pub seal: SealBitmap,
    /// Chunk-index → durable manifest hash at the dest's restore manifest
    /// (the last checkpoint chain manifest). `AltSource` comparisons.
    pub durable_at: Vec<Option<ChunkHash>>,
    /// `GetChunk` gate — the same full-session-manifest allowlist the
    /// gRPC `MigrationFetch` uses.
    pub allowed_chunks: HashSet<ChunkHash>,
    pub chunk_size: u64,
    pub total_bytes: u64,
    /// Per-leg serve attribution (logged at DrainDone): where the
    /// per-request wall actually goes on this no-SHA-NI fleet.
    pub serve: ServeStats,
    /// Set when the dest reports `DrainDone` — after this the source FC
    /// is no longer needed as a page source (commit may proceed).
    pub drained: AtomicBool,
}

/// What one sealed chunk serves as. Pure classification — unit-tested
/// apart from the readv plumbing.
#[derive(Debug, PartialEq, Eq)]
pub enum ServedChunk {
    /// Whole chunk is zero bytes.
    Zero,
    /// Live bytes == the durable manifest entry: dest fetches class-2.
    AltSource(ChunkHash),
    /// Peer-authoritative bytes (with their hash, for dest-side verify).
    Page(Vec<u8>, ChunkHash),
}

/// Cumulative per-leg serve timings (µs) + counts. All relaxed
/// atomics — observability only.
#[derive(Default)]
pub struct ServeStats {
    pub fault_serves: std::sync::atomic::AtomicU64,
    pub drain_serves: std::sync::atomic::AtomicU64,
    pub readv_us: std::sync::atomic::AtomicU64,
    pub classify_us: std::sync::atomic::AtomicU64,
    pub encode_us: std::sync::atomic::AtomicU64,
    pub write_us: std::sync::atomic::AtomicU64,
}

/// Classify a chunk's live bytes against the durable manifest entry.
pub fn classify_served_chunk(bytes: Vec<u8>, durable: Option<&ChunkHash>) -> ServedChunk {
    if bytes.iter().all(|b| *b == 0) {
        return ServedChunk::Zero;
    }
    let hash = ChunkHash::of(&bytes);
    if durable == Some(&hash) {
        return ServedChunk::AltSource(hash);
    }
    ServedChunk::Page(bytes, hash)
}

/// Read `[file_offset, file_offset + len)` of guest memory from the
/// paused FC process, splitting across the covering VMAs. Errors if any
/// part of the range is uncovered — impossible for sealed chunks (dirty
/// pages imply a mapped VMA), so a hit means a protocol/scan bug and
/// must be loud.
#[cfg(target_os = "linux")]
pub fn read_guest_range(
    pid: u32,
    vmas: &[GuestVma],
    file_offset: u64,
    len: usize,
) -> std::io::Result<Vec<u8>> {
    use std::io::IoSliceMut;

    use nix::sys::uio::{process_vm_readv, RemoteIoVec};
    use nix::unistd::Pid;

    let mut out = vec![0u8; len];
    let mut filled = 0usize;
    while filled < len {
        let off = file_offset + filled as u64;
        let vma = vmas
            .iter()
            .find(|v| v.file_offset <= off && off < v.file_offset + v.len())
            .ok_or_else(|| {
                std::io::Error::other(format!(
                    "guest offset {off:#x} not covered by any base-shm VMA (scan/protocol bug)"
                ))
            })?;
        let va = vma.start + (off - vma.file_offset);
        let n = (len - filled).min((vma.end - va) as usize);
        let mut local = [IoSliceMut::new(&mut out[filled..filled + n])];
        let remote = [RemoteIoVec {
            base: va as usize,
            len: n,
        }];
        let got = process_vm_readv(Pid::from_raw(pid as i32), &mut local, &remote)
            .map_err(|e| std::io::Error::other(format!("process_vm_readv pid {pid}: {e}")))?;
        if got == 0 {
            return Err(std::io::Error::other(format!(
                "process_vm_readv pid {pid} returned 0 at va {va:#x}"
            )));
        }
        filled += got;
    }
    Ok(out)
}

/// Non-Linux stub (the fleet is Linux; VZ dev never migrates).
#[cfg(not(target_os = "linux"))]
pub fn read_guest_range(
    _pid: u32,
    _vmas: &[GuestVma],
    _file_offset: u64,
    _len: usize,
) -> std::io::Result<Vec<u8>> {
    Err(std::io::Error::other(
        "post-copy page serving is Linux-only",
    ))
}

/// The page server: one per host-agent, holding the live exports.
pub struct PeerServer {
    /// The port the listener binds (presetup advertises it to the
    /// coordinator, which pairs it with the source's host address).
    port: u16,
    /// How long an unknown-export `Hello` parks awaiting the capture's
    /// registration (export-TTL scale in production; tests shrink it).
    park_budget: std::time::Duration,
    exports: DashMap<String, Arc<PeerExport>>,
    /// `GetChunk` backing. `None` ⇒ `GetChunk` answers `Error` (the dest
    /// falls back to GCS) — hosts without chunk machinery can't be
    /// migration sources anyway.
    cache: Option<ChunkCache>,
    store: Option<ChunkStore>,
}

impl PeerServer {
    pub fn new(port: u16, cache: Option<ChunkCache>, store: Option<ChunkStore>) -> Arc<Self> {
        Self::new_with_park_budget(port, cache, store, std::time::Duration::from_secs(120))
    }

    /// Test seam: shrink the unknown-export park window.
    pub fn new_with_park_budget(
        port: u16,
        cache: Option<ChunkCache>,
        store: Option<ChunkStore>,
        park_budget: std::time::Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            port,
            park_budget,
            exports: DashMap::new(),
            cache,
            store,
        })
    }

    /// The listener port (the coordinator pairs it with the source's
    /// advertised host address).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Register a capture's export. Replaces any stale entry with the
    /// same id (cannot happen — export ids are single-use nonces — but
    /// last-write-wins beats a panic in the capture path).
    pub fn register(&self, export: PeerExport) {
        self.exports
            .insert(export.export_id.clone(), Arc::new(export));
    }

    /// Remove an export at commit/abort. Idempotent.
    pub fn remove(&self, export_id: &str) -> Option<Arc<PeerExport>> {
        self.exports.remove(export_id).map(|(_, e)| e)
    }

    pub fn get(&self, export_id: &str) -> Option<Arc<PeerExport>> {
        self.exports.get(export_id).map(|e| e.clone())
    }

    /// Bind `addr` and serve connections until the task is dropped.
    /// Inert without registered exports.
    pub async fn serve(self: Arc<Self>, addr: SocketAddr) -> std::io::Result<()> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(%addr, "migrate-peer page server listening");
        self.serve_on(listener).await
    }

    /// Serve connections off an already-bound listener (tests bind their
    /// own ephemeral port to dodge the pick-then-bind race).
    pub async fn serve_on(
        self: Arc<Self>,
        listener: tokio::net::TcpListener,
    ) -> std::io::Result<()> {
        loop {
            let (stream, peer) = listener.accept().await?;
            let server = self.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                let std_stream = match stream.into_std() {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "peer conn into_std failed");
                        return;
                    }
                };
                // The codec is sync; undo tokio's nonblocking mode.
                if let Err(e) = std_stream.set_nonblocking(false) {
                    tracing::warn!(error = %e, "peer conn set_nonblocking failed");
                    return;
                }
                let _ = std_stream.set_nodelay(true);
                if let Err(e) = server.handle_conn(std_stream, handle) {
                    tracing::debug!(%peer, error = %e, "peer conn ended with error");
                }
            });
        }
    }

    /// One connection: Hello → HelloAck → Seal push → request loop.
    fn handle_conn(
        &self,
        mut stream: std::net::TcpStream,
        handle: tokio::runtime::Handle,
    ) -> std::io::Result<()> {
        let hello: ToSource = read_frame(&mut stream)?;
        let (export, purpose) = match hello {
            ToSource::Hello {
                version,
                export_id,
                token,
                purpose,
            } => {
                if version != PROTO_VERSION {
                    write_frame(
                        &mut stream,
                        &FromSource::Error {
                            req_id: None,
                            message: format!(
                                "proto version mismatch: got {version}, want {PROTO_VERSION}"
                            ),
                        },
                    )?;
                    return Ok(());
                }
                // ADR 0045 C2: PARK an unknown export instead of
                // rejecting — the destination handler is spawned (and
                // dials) BEFORE the source pauses; its export appears
                // only when the capture registers it. Poll-park up to
                // the export-TTL scale; sub-ms wakeup once the seal
                // lands keeps the blackout honest. Bounded + VPC-
                // internal + one-migration-per-host (R8), so the
                // parked-conn surface is tiny. (Found by prod-probing
                // the Hello path: the reject made every first-attempt
                // C2 move fail by construction.)
                let park_budget = self.park_budget;
                let parked_at = std::time::Instant::now();
                let export = loop {
                    if let Some(export) = self.get(&export_id) {
                        break export;
                    }
                    if parked_at.elapsed() > park_budget {
                        write_frame(
                            &mut stream,
                            &FromSource::Error {
                                req_id: None,
                                message: "unknown export (park budget exhausted)".into(),
                            },
                        )?;
                        return Ok(());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                };
                // Hash-then-compare: constant-time without a new dep.
                let got = Sha256::digest(token.as_bytes());
                let want = Sha256::digest(export.token.as_bytes());
                if got != want {
                    write_frame(
                        &mut stream,
                        &FromSource::Error {
                            req_id: None,
                            message: "bad token".into(),
                        },
                    )?;
                    return Ok(());
                }
                (export, purpose)
            }
            other => {
                write_frame(
                    &mut stream,
                    &FromSource::Error {
                        req_id: None,
                        message: format!("expected Hello first, got {other:?}"),
                    },
                )?;
                return Ok(());
            }
        };

        write_frame(
            &mut stream,
            &FromSource::HelloAck {
                version: PROTO_VERSION,
                chunk_size: export.chunk_size,
                total_bytes: export.total_bytes,
            },
        )?;
        write_frame(
            &mut stream,
            &FromSource::Seal {
                bitmap: export.seal.clone(),
            },
        )?;

        loop {
            let req: ToSource = match read_frame(&mut stream) {
                Ok(r) => r,
                // Dest closed (normal teardown for the fault conn).
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e),
            };
            match req {
                ToSource::NeedAt {
                    req_id,
                    chunk_offset,
                } => {
                    let resp = self.serve_need_at(&export, req_id, chunk_offset, purpose);
                    let t_write = std::time::Instant::now();
                    write_frame(&mut stream, &resp)?;
                    export
                        .serve
                        .write_us
                        .fetch_add(t_write.elapsed().as_micros() as u64, Ordering::Relaxed);
                }
                ToSource::GetChunk { req_id, hash } => {
                    let resp = self.serve_get_chunk(&export, req_id, hash, &handle);
                    write_frame(&mut stream, &resp)?;
                }
                ToSource::DrainDone {
                    pulled,
                    alt_sourced,
                    zero_chunks,
                } => {
                    export.drained.store(true, Ordering::SeqCst);
                    let st = &export.serve;
                    tracing::info!(
                        export_id = %export.export_id,
                        sandbox_id = %export.sandbox_id,
                        pulled,
                        alt_sourced,
                        zero_chunks,
                        fault_serves = st.fault_serves.load(Ordering::Relaxed),
                        drain_serves = st.drain_serves.load(Ordering::Relaxed),
                        readv_us = st.readv_us.load(Ordering::Relaxed),
                        classify_us = st.classify_us.load(Ordering::Relaxed),
                        encode_us = st.encode_us.load(Ordering::Relaxed),
                        write_us = st.write_us.load(Ordering::Relaxed),
                        "post-copy drain complete (dest-reported; serve-leg attribution)"
                    );
                    return Ok(());
                }
                ToSource::Hello { .. } => {
                    write_frame(
                        &mut stream,
                        &FromSource::Error {
                            req_id: None,
                            message: "duplicate Hello".into(),
                        },
                    )?;
                    return Ok(());
                }
            }
        }
    }

    fn serve_need_at(
        &self,
        export: &PeerExport,
        req_id: u64,
        chunk_offset: u64,
        purpose: engram_migrate_proto::ConnPurpose,
    ) -> FromSource {
        use std::sync::atomic::Ordering::Relaxed;
        if !chunk_offset.is_multiple_of(export.chunk_size) || chunk_offset >= export.total_bytes {
            return FromSource::Error {
                req_id: Some(req_id),
                message: format!("NeedAt offset {chunk_offset:#x} unaligned or out of range"),
            };
        }
        let idx = chunk_offset / export.chunk_size;
        if !export.seal.get(idx) {
            // Unsealed NeedAt = a protocol bug on the dest, not a race
            // (the bitmap ships before any fault is served).
            return FromSource::Error {
                req_id: Some(req_id),
                message: format!("NeedAt for unsealed chunk {idx}"),
            };
        }
        let latency_critical = matches!(purpose, engram_migrate_proto::ConnPurpose::Fault);
        if latency_critical {
            export.serve.fault_serves.fetch_add(1, Relaxed);
        } else {
            export.serve.drain_serves.fetch_add(1, Relaxed);
        }
        let len = export.chunk_size.min(export.total_bytes - chunk_offset) as usize;
        let t_readv = std::time::Instant::now();
        let bytes = match read_guest_range(export.fc_pid, &export.vmas, chunk_offset, len) {
            Ok(b) => b,
            Err(e) => {
                return FromSource::Error {
                    req_id: Some(req_id),
                    message: format!("readv failed: {e}"),
                };
            }
        };
        export
            .serve
            .readv_us
            .fetch_add(t_readv.elapsed().as_micros() as u64, Relaxed);

        // v3: the AltSource classify (sha256 of the raw 512 KiB, ~1 ms
        // on this no-SHA-NI fleet) runs ONLY for drain serves. On the
        // fault path a demote is strictly WORSE than shipping the
        // resident bytes — it converts one stalled-vCPU round trip
        // into a dest-side cache/GCS fetch. The zero check stays on
        // both paths (cheap scan, saves the whole payload).
        let t_classify = std::time::Instant::now();
        let classified = if latency_critical {
            if bytes.iter().all(|b| *b == 0) {
                ServedChunk::Zero
            } else {
                // The raw hash is unused on this arm — Page integrity
                // is computed over the wire bytes below.
                ServedChunk::Page(bytes, ChunkHash::from_bytes([0u8; 32]))
            }
        } else {
            let durable = export.durable_at.get(idx as usize).and_then(|d| d.as_ref());
            classify_served_chunk(bytes, durable)
        };
        export
            .serve
            .classify_us
            .fetch_add(t_classify.elapsed().as_micros() as u64, Relaxed);

        match classified {
            ServedChunk::Zero => FromSource::ZeroChunk {
                req_id,
                chunk_offset,
            },
            ServedChunk::AltSource(hash) => FromSource::AltSource {
                req_id,
                chunk_offset,
                durable_sha256: *hash.as_bytes(),
            },
            ServedChunk::Page(bytes, _raw_hash) => {
                // v2: ship the smaller of lz4/raw; integrity covers
                // the wire bytes (and hashing the smaller payload is
                // itself a win on this no-SHA-NI fleet).
                let t_encode = std::time::Instant::now();
                let (wire, lz4) = engram_migrate_proto::compress_page(bytes);
                let hash = engram_migrate_proto::wire_hash(&wire);
                export
                    .serve
                    .encode_us
                    .fetch_add(t_encode.elapsed().as_micros() as u64, Relaxed);
                FromSource::Page {
                    req_id,
                    chunk_offset,
                    bytes: wire,
                    hash,
                    lz4,
                }
            }
        }
    }

    fn serve_get_chunk(
        &self,
        export: &PeerExport,
        req_id: u64,
        hash: [u8; 32],
        handle: &tokio::runtime::Handle,
    ) -> FromSource {
        let hash = ChunkHash::from_bytes(hash);
        if !export.allowed_chunks.contains(&hash) {
            return FromSource::Error {
                req_id: Some(req_id),
                message: "chunk not in export allowlist".into(),
            };
        }
        let (Some(cache), Some(store)) = (self.cache.as_ref(), self.store.as_ref()) else {
            return FromSource::Error {
                req_id: Some(req_id),
                message: "source has no chunk cache/store".into(),
            };
        };
        let store = store.clone();
        match handle.block_on(cache.get(hash, move || {
            let store = store.clone();
            async move { store.get_chunk(hash).await }
        })) {
            Ok(bytes) => FromSource::ChunkBytes {
                req_id,
                bytes: bytes.to_vec(),
            },
            Err(e) => FromSource::Error {
                req_id: Some(req_id),
                message: format!("chunk fetch failed: {e}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    fn hash_of(bytes: &[u8]) -> ChunkHash {
        ChunkHash::of(bytes)
    }

    #[test]
    fn classification_arms() {
        // All-zero wins even when the durable entry matches.
        let zeros = vec![0u8; 64];
        let zh = hash_of(&zeros);
        assert_eq!(classify_served_chunk(zeros, Some(&zh)), ServedChunk::Zero);

        let data = vec![7u8; 64];
        let dh = hash_of(&data);
        assert_eq!(
            classify_served_chunk(data.clone(), Some(&dh)),
            ServedChunk::AltSource(dh)
        );
        assert_eq!(
            classify_served_chunk(data.clone(), None),
            ServedChunk::Page(data.clone(), dh)
        );
        let other = hash_of(&[1u8]);
        assert_eq!(
            classify_served_chunk(data.clone(), Some(&other)),
            ServedChunk::Page(data, dh)
        );
    }

    fn test_export(token: &str) -> PeerExport {
        let mut seal = SealBitmap::new(4096, 4);
        seal.set(1);
        PeerExport {
            export_id: "exp-1".into(),
            token: token.into(),
            sandbox_id: SandboxId::new(),
            // Our own pid: auth/validation tests never reach readv.
            fc_pid: std::process::id(),
            vmas: vec![],
            seal,
            durable_at: vec![None; 4],
            allowed_chunks: HashSet::new(),
            chunk_size: 4096,
            total_bytes: 4 * 4096,
            serve: Default::default(),
            drained: AtomicBool::new(false),
        }
    }

    /// Drive one client conversation against a served `PeerServer` over
    /// loopback TCP; returns the frames received after writing `msgs`.
    async fn talk(server: Arc<PeerServer>, msgs: Vec<ToSource>, expect: usize) -> Vec<FromSource> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serve = tokio::spawn(server.serve_on(listener));
        let mut stream = std::net::TcpStream::connect(addr).expect("dial peer server");
        let out = tokio::task::spawn_blocking(move || {
            for m in &msgs {
                write_frame(&mut stream, m).unwrap();
            }
            stream.flush().unwrap();
            let mut got = Vec::new();
            for _ in 0..expect {
                match read_frame::<_, FromSource>(&mut stream) {
                    Ok(f) => got.push(f),
                    Err(_) => break,
                }
            }
            got
        })
        .await
        .unwrap();
        serve.abort();
        out
    }

    fn hello(export_id: &str, token: &str) -> ToSource {
        ToSource::Hello {
            version: PROTO_VERSION,
            export_id: export_id.into(),
            token: token.into(),
            purpose: engram_migrate_proto::ConnPurpose::Fault,
        }
    }

    #[tokio::test]
    async fn unknown_export_and_bad_token_are_rejected() {
        let server = PeerServer::new_with_park_budget(
            9102,
            None,
            None,
            std::time::Duration::from_millis(50),
        );
        server.register(test_export("right"));

        let got = talk(server.clone(), vec![hello("nope", "right")], 1).await;
        assert!(
            matches!(&got[0], FromSource::Error { req_id: None, message } if message.contains("unknown export"))
        );

        let got = talk(server.clone(), vec![hello("exp-1", "wrong")], 1).await;
        assert!(
            matches!(&got[0], FromSource::Error { req_id: None, message } if message.contains("bad token"))
        );
    }

    #[tokio::test]
    async fn version_skew_is_rejected() {
        let server = PeerServer::new(9102, None, None);
        server.register(test_export("t"));
        let got = talk(
            server,
            vec![ToSource::Hello {
                version: PROTO_VERSION + 1,
                export_id: "exp-1".into(),
                token: "t".into(),
                purpose: engram_migrate_proto::ConnPurpose::Fault,
            }],
            1,
        )
        .await;
        assert!(
            matches!(&got[0], FromSource::Error { req_id: None, message } if message.contains("version mismatch"))
        );
    }

    #[tokio::test]
    async fn good_hello_acks_and_pushes_seal_then_validates_need_at() {
        let server = PeerServer::new(9102, None, None);
        server.register(test_export("t"));
        let got = talk(
            server,
            vec![
                hello("exp-1", "t"),
                // Unaligned.
                ToSource::NeedAt {
                    req_id: 1,
                    chunk_offset: 17,
                },
                // Aligned but unsealed (only chunk 1 is sealed).
                ToSource::NeedAt {
                    req_id: 2,
                    chunk_offset: 0,
                },
                // Out of range.
                ToSource::NeedAt {
                    req_id: 3,
                    chunk_offset: 4 * 4096,
                },
            ],
            5,
        )
        .await;
        assert!(matches!(
            &got[0],
            FromSource::HelloAck {
                version: PROTO_VERSION,
                chunk_size: 4096,
                total_bytes: 16384,
            }
        ));
        let FromSource::Seal { bitmap } = &got[1] else {
            panic!("expected Seal push, got {:?}", got[1]);
        };
        bitmap.validate().unwrap();
        assert!(bitmap.get(1) && !bitmap.get(0));
        assert!(
            matches!(&got[2], FromSource::Error { req_id: Some(1), message } if message.contains("unaligned"))
        );
        assert!(
            matches!(&got[3], FromSource::Error { req_id: Some(2), message } if message.contains("unsealed"))
        );
        assert!(
            matches!(&got[4], FromSource::Error { req_id: Some(3), message } if message.contains("unaligned or out of range"))
        );
    }

    #[tokio::test]
    async fn get_chunk_outside_allowlist_is_rejected() {
        let server = PeerServer::new(9102, None, None);
        server.register(test_export("t"));
        let got = talk(
            server,
            vec![
                hello("exp-1", "t"),
                ToSource::GetChunk {
                    req_id: 9,
                    hash: [0xEE; 32],
                },
            ],
            3,
        )
        .await;
        assert!(
            matches!(&got[2], FromSource::Error { req_id: Some(9), message } if message.contains("allowlist"))
        );
    }

    /// ADR 0045 C2: the pre-staged destination dials BEFORE the
    /// capture registers the export — the server PARKS the Hello and
    /// completes the handshake the moment registration lands. (The
    /// immediate-reject this replaces made every first-attempt
    /// post-copy move fail by construction; found prod-probing.)
    #[tokio::test]
    async fn early_hello_parks_until_the_export_registers() {
        let server = PeerServer::new(9102, None, None);
        let registrar = server.clone();
        // Register 150ms AFTER the Hello is in flight.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            registrar.register(test_export("t"));
        });
        let got = talk(server, vec![hello("exp-1", "t")], 2).await;
        assert!(
            matches!(&got[0], FromSource::HelloAck { .. }),
            "parked hello must ack once the export registers, got {:?}",
            got[0]
        );
        assert!(matches!(&got[1], FromSource::Seal { .. }));
    }

    #[tokio::test]
    async fn drain_done_marks_export_and_closes() {
        let server = PeerServer::new(9102, None, None);
        server.register(test_export("t"));
        let _ = talk(
            server.clone(),
            vec![
                hello("exp-1", "t"),
                ToSource::DrainDone {
                    pulled: 1,
                    alt_sourced: 0,
                    zero_chunks: 0,
                },
            ],
            2,
        )
        .await;
        // `talk` returns once the CLIENT has its two reply frames
        // (HelloAck + Seal); the server's blocking conn loop may not
        // have consumed the trailing DrainDone yet. Poll with a
        // deadline instead of asserting instantly (flaked on CI,
        // 2026-06-12).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if server.get("exp-1").unwrap().drained.load(Ordering::SeqCst) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "DrainDone never marked the export drained",
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}
