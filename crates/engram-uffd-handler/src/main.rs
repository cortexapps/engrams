//! `engram-uffd-handler` — page-fault handler binary.
//!
//! ADR 0020 Route B chunk-native shape. Per restore, FC connects to
//! the handler's UDS, hands over the userfaultfd + JSON mappings, and
//! the handler serves every fault by resolving its offset against a
//! [`ChunkedMemoryBackend`] and `UFFDIO_COPY`-ing the resolved chunk
//! from the chunk cache/store (zero-filled chunks via
//! `UFFDIO_ZEROPAGE`). There is no `memory.bin` file.
//!
//! With `--base-shm` (ADR 0045 substrate), canonical (un-diverged) pages
//! install via `UFFDIO_CONTINUE` over a shared per-template base file —
//! one host-wide copy mapped `MAP_PRIVATE` by every fork — while divergent
//! pages stay private via `UFFDIO_COPY`.
//!
//! ```text
//! engram-uffd-handler \
//!   --listen /tmp/uffd.sock \
//!   --canonical-manifest <uuid>@v<n> \
//!   --session-manifest   <uuid>@v<n> \
//!   [--fault-around-chunks N] (env ENGRAM_UFFD_FAULT_AROUND, default 8)
//!   [--trace-key <session-uuid>] \
//!   [--report-dir <dir>] \
//!   [--cache-root /var/cache/engram/chunks]
//! ```
//!
//! ADR 0070: this handler is a single-evictor-safe cache citizen — it
//! *populates* the shared `cache_root` (write-through of faulted chunks,
//! the locality win) but never *evicts* from it
//! (`ChunkCacheConfig::eviction_enabled: false`). The host-agent, which
//! holds the pin set, is the one process per host that evicts; running a
//! second, pin-blind LRU policy here used to let a pressured handler
//! sweep reclaim exactly the pinned base-image chunks the host-agent was
//! protecting.
//!
//! `ENGRAM_BLOB_BACKEND` (and `ENGRAM_GCS_BUCKET`) pick the blob
//! backend — same convention every Engram crate follows so chunked
//! reads work in dev (local-fs) and prod (GCS) without code changes.
//!
//! Linux-only at runtime; on other platforms the binary still
//! compiles (it's part of a workspace `cargo check` that may run
//! on macOS) but exits 2 with a clear message.

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("engram-uffd-handler: userfaultfd is a Linux kernel feature; this binary is a no-op on other platforms");
    std::process::ExitCode::from(2)
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    use tracing::Instrument;

    let args = match linux::parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("engram-uffd-handler: build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    // Run telemetry init + the listener *inside* the runtime. The OTLP batch
    // exporter `engram_telemetry::init` builds spawns a background task and so
    // must be constructed from a runtime context (otherwise hyper-util panics
    // "there is no reactor running"); and the guard's shutdown-flush on drop
    // needs the runtime still alive. The guard is the last thing to drop in
    // the async block, so the flush runs while the runtime is up. OTLP is
    // inert unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
    rt.block_on(async move {
        let _telemetry = engram_telemetry::init(engram_telemetry::Config {
            service_name: "engram-uffd-handler",
            default_filter: "info",
        });

        // ADR 0019: root this process's spans on the host-agent's restore
        // span, whose `traceparent` the FC backend handed us via the env.
        // No-op when unset (OTLP off) or malformed.
        let span = tracing::info_span!("uffd.run");
        if let Ok(tp) = std::env::var("TRACEPARENT") {
            engram_telemetry::set_parent_from_traceparent(&span, &tp);
        }

        let handle = tokio::runtime::Handle::current();
        match linux::run(args, handle).instrument(span).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("engram-uffd-handler: {e}");
                ExitCode::from(1)
            }
        }
    })
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use engram_core::traits::BlobStorage;
    use engram_core::types::manifest::ManifestRef;
    use engram_uffd_handler::ChunkedMemoryBackend;
    use tokio::runtime::Handle as TokioHandle;
    use tracing::Instrument as _;
    use uuid::Uuid;

    pub struct Args {
        pub listen: PathBuf,
        pub canonical_manifest: ManifestRef,
        pub session_manifest: ManifestRef,
        /// Read the CANONICAL manifest from this local JSON file instead
        /// of the blob store (the host stages a copy beside the snapshot).
        pub canonical_manifest_json: Option<PathBuf>,
        /// Read the SESSION manifest from this local JSON file instead of
        /// the blob store: a migration destination's not-yet-durable
        /// manifest, or the host's staged copy.
        pub session_manifest_json: Option<PathBuf>,
        /// Peer mode: the hot-set hint the destination stages (see
        /// [`engram_uffd_handler::hot_hint`]). It orders the drain.
        pub migration_hint: Option<PathBuf>,
        /// The session id: the handler reads the session's published
        /// first-touch prefix under it.
        pub trace_key: Option<Uuid>,
        /// Where the handler writes its first-touch record and, in peer
        /// mode, its drain stats (the jail dir).
        pub report_dir: Option<PathBuf>,
        /// ADR 0014 M1.14: when set, bypass `pick_blob_backend`'s
        /// env-var lookup and `LocalBlobStorage::new(this)` directly.
        /// Used by the bake/materialize profile pass, where the bake
        /// already knows its chunk-store root and the UFFD handler
        /// needs to read from the same place — env-var-driven
        /// resolution would mis-locate it because the bake uses a
        /// `<images_dir>/store/` layout (not `<root>/blobs/`).
        pub blob_root: Option<PathBuf>,
        pub cache_root: PathBuf,
        /// ADR 0045 substrate (v2b): the per-template base shm file this
        /// handler creates, sizes, and lazily populates with canonical
        /// chunks. The forked FC maps it MAP_PRIVATE and registers
        /// MISSING|MINOR; canonical faults resolve via UFFDIO_CONTINUE
        /// (shared), divergent via UFFDIO_COPY (private). Off when unset.
        pub base_shm: Option<PathBuf>,
        /// Where to record the base shm file this handler actually opened
        /// (empty when it falls back to running without one). The host
        /// reads it to choose the Firecracker load's base file.
        pub base_shm_ack: Option<PathBuf>,
        /// ADR 0045 C2 (post-copy destination): `host:port` of the SOURCE
        /// host-agent's page server. With `--peer-export-id`, arms peer
        /// mode: connect + authenticate + block for the `Seal` BEFORE
        /// binding the FC-facing UDS (so "FC can load" ⇒ "seal held"),
        /// then serve sealed faults from the peer and drain the rest in
        /// the background. The token rides `ENGRAM_PEER_TOKEN` (argv
        /// leaks via /proc/*/cmdline); `--peer-token` is a dev override.
        pub peer_addr: Option<String>,
        pub fault_around_chunks: usize,
        pub peer_export_id: Option<String>,
        pub peer_token: Option<String>,
        /// ADR 0045 C2: bind a UnixListener here and stream one-way
        /// `HandlerControl` reports (Sealed/DrainProgress/DrainDone/
        /// PeerLost) to whichever host-agent dials in.
        pub control_sock: Option<PathBuf>,
        /// Peer mode: write every peer-supplied chunk here, at its
        /// session offset; the file gets this name once the drain is done.
        pub sealed_output: Option<PathBuf>,
        /// ADR 0075: the substrate populate socket (the single-writer
        /// host-agent). When set, chunk misses are populated by the
        /// writer (its singleflight / pins / budget govern) and this
        /// handler performs the readiness Hello BEFORE creating the
        /// base-shm file or binding the FC-facing UDS. When unset
        /// (tests, the bake/materialize profile pass), misses take the
        /// direct-blob fallback.
        pub substrate_sock: Option<PathBuf>,
    }

    pub fn parse_args() -> Result<Args, String> {
        let mut listen: Option<PathBuf> = None;
        let mut canonical_manifest: Option<ManifestRef> = None;
        let mut session_manifest: Option<ManifestRef> = None;
        let mut canonical_manifest_json: Option<PathBuf> = None;
        let mut session_manifest_json: Option<PathBuf> = None;
        let mut migration_hint: Option<PathBuf> = None;
        let mut trace_key: Option<Uuid> = None;
        let mut report_dir: Option<PathBuf> = None;
        let mut blob_root: Option<PathBuf> = None;
        let mut cache_root: Option<PathBuf> = None;
        let mut base_shm: Option<PathBuf> = None;
        let mut base_shm_ack: Option<PathBuf> = None;
        let mut fault_around_chunks = std::env::var("ENGRAM_UFFD_FAULT_AROUND")
            .unwrap_or_else(|_| engram_uffd_handler::peer::DEFAULT_FAULT_AROUND_CHUNKS.to_string());
        let mut peer_addr: Option<String> = None;
        let mut peer_export_id: Option<String> = None;
        let mut peer_token: Option<String> = None;
        let mut control_sock: Option<PathBuf> = None;
        let mut sealed_output: Option<PathBuf> = None;
        let mut substrate_sock: Option<PathBuf> = None;

        let mut argv = std::env::args().skip(1);
        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "--fault-around-chunks" => {
                    fault_around_chunks = argv
                        .next()
                        .ok_or("--fault-around-chunks requires a value")?;
                }
                "--listen" => {
                    listen = Some(PathBuf::from(
                        argv.next()
                            .ok_or_else(|| "--listen requires a value".to_string())?,
                    ));
                }
                "--canonical-manifest" => {
                    let v = argv
                        .next()
                        .ok_or_else(|| "--canonical-manifest requires a value".to_string())?;
                    canonical_manifest = Some(parse_manifest_ref(&v)?);
                }
                "--session-manifest" => {
                    let v = argv
                        .next()
                        .ok_or_else(|| "--session-manifest requires a value".to_string())?;
                    session_manifest = Some(parse_manifest_ref(&v)?);
                }
                "--canonical-manifest-json" => {
                    let v = argv
                        .next()
                        .ok_or_else(|| "--canonical-manifest-json requires a value".to_string())?;
                    canonical_manifest_json = Some(PathBuf::from(v));
                }
                "--session-manifest-json" => {
                    let v = argv
                        .next()
                        .ok_or_else(|| "--session-manifest-json requires a value".to_string())?;
                    session_manifest_json = Some(PathBuf::from(v));
                }
                "--migration-hint" => {
                    migration_hint =
                        Some(PathBuf::from(argv.next().ok_or_else(|| {
                            "--migration-hint requires a value".to_string()
                        })?));
                }
                "--trace-key" => {
                    let v = argv
                        .next()
                        .ok_or_else(|| "--trace-key requires a value".to_string())?;
                    trace_key =
                        Some(Uuid::parse_str(&v).map_err(|e| format!("--trace-key {v:?}: {e}"))?);
                }
                "--report-dir" => {
                    report_dir =
                        Some(PathBuf::from(argv.next().ok_or_else(|| {
                            "--report-dir requires a value".to_string()
                        })?));
                }
                "--blob-root" => {
                    blob_root = Some(PathBuf::from(
                        argv.next()
                            .ok_or_else(|| "--blob-root requires a value".to_string())?,
                    ));
                }
                "--cache-root" => {
                    cache_root =
                        Some(PathBuf::from(argv.next().ok_or_else(|| {
                            "--cache-root requires a value".to_string()
                        })?));
                }
                "--base-shm" => {
                    base_shm = Some(PathBuf::from(
                        argv.next()
                            .ok_or_else(|| "--base-shm requires a value".to_string())?,
                    ));
                }
                "--base-shm-ack" => {
                    base_shm_ack =
                        Some(PathBuf::from(argv.next().ok_or_else(|| {
                            "--base-shm-ack requires a value".to_string()
                        })?));
                }
                "--peer-addr" => {
                    peer_addr = Some(
                        argv.next()
                            .ok_or_else(|| "--peer-addr requires a value".to_string())?,
                    );
                }
                "--peer-export-id" => {
                    peer_export_id = Some(
                        argv.next()
                            .ok_or_else(|| "--peer-export-id requires a value".to_string())?,
                    );
                }
                "--peer-token" => {
                    peer_token = Some(
                        argv.next()
                            .ok_or_else(|| "--peer-token requires a value".to_string())?,
                    );
                }
                "--control-sock" => {
                    control_sock =
                        Some(PathBuf::from(argv.next().ok_or_else(|| {
                            "--control-sock requires a value".to_string()
                        })?));
                }
                "--sealed-output" => {
                    sealed_output =
                        Some(PathBuf::from(argv.next().ok_or_else(|| {
                            "--sealed-output requires a value".to_string()
                        })?));
                }
                "--substrate-sock" => {
                    substrate_sock =
                        Some(PathBuf::from(argv.next().ok_or_else(|| {
                            "--substrate-sock requires a value".to_string()
                        })?));
                }
                "-h" | "--help" => {
                    eprintln!("{HELP}");
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }

        let listen = listen.ok_or_else(|| "--listen <sock> is required".to_string())?;
        let canonical_manifest = canonical_manifest
            .ok_or_else(|| "--canonical-manifest <uuid>@v<n> is required".to_string())?;
        let session_manifest = session_manifest
            .ok_or_else(|| "--session-manifest <uuid>@v<n> is required".to_string())?;
        let cache_root = cache_root.unwrap_or_else(|| PathBuf::from("/var/cache/engram/chunks"));

        // Peer mode is all-or-nothing: addr + export id together, the
        // token from the env (or the dev-override flag).
        if peer_addr.is_some() != peer_export_id.is_some() {
            return Err("--peer-addr and --peer-export-id are required together".to_string());
        }
        if peer_addr.is_some() {
            if peer_token.is_none() {
                peer_token = std::env::var("ENGRAM_PEER_TOKEN")
                    .ok()
                    .filter(|t| !t.is_empty());
            }
            if peer_token.is_none() {
                return Err(
                    "peer mode needs a token: set ENGRAM_PEER_TOKEN (or --peer-token in dev)"
                        .to_string(),
                );
            }
        }

        let fault_around_chunks: usize = fault_around_chunks
            .parse()
            .map_err(|_| "invalid fault-around chunk count")?;
        if !(1..=engram_migrate_proto::MAX_FAULT_AROUND_CHUNKS).contains(&fault_around_chunks) {
            return Err("fault-around chunk count must be in 1..=256".into());
        }
        Ok(Args {
            fault_around_chunks,
            listen,
            canonical_manifest,
            session_manifest,
            canonical_manifest_json,
            session_manifest_json,
            migration_hint,
            trace_key,
            report_dir,
            blob_root,
            cache_root,
            base_shm,
            base_shm_ack,
            peer_addr,
            peer_export_id,
            peer_token,
            control_sock,
            sealed_output,
            substrate_sock,
        })
    }

    /// Accept `<uuid>@v<num>` (canonical Display form) or
    /// `<uuid>:<num>` (the looser dev-ops form that's easier to
    /// type by hand).
    fn parse_manifest_ref(s: &str) -> Result<ManifestRef, String> {
        let (id_part, version_part) = match s.split_once("@v") {
            Some(p) => p,
            None => s.split_once(':').ok_or_else(|| {
                format!("manifest ref {s:?} must be <uuid>@v<num> or <uuid>:<num>")
            })?,
        };
        let manifest_id =
            Uuid::parse_str(id_part).map_err(|e| format!("manifest ref {s:?} uuid: {e}"))?;
        let version = version_part
            .parse::<u64>()
            .map_err(|e| format!("manifest ref {s:?} version: {e}"))?;
        Ok(ManifestRef {
            manifest_id,
            version,
        })
    }

    pub async fn run(args: Args, handle: TokioHandle) -> Result<(), String> {
        // ADR 0075: dial the single writer and run the readiness Hello
        // BEFORE any FC-facing surface exists (base-shm creation + the
        // UDS bind happen downstream of here). A HelloAck with
        // tmpfs_ok=false exits nonzero with a distinct string the
        // host-agent surfaces in the spawn failure — the VM fails at
        // spawn time instead of booting and later dying with
        // `register memory … userfaultfd … System error`. The Hello runs
        // beside the blob backend's setup; neither needs the other.
        let hello = args.substrate_sock.as_ref().map(|sock| {
            let client = std::sync::Arc::new(
                engram_uffd_handler::populate_client::PopulateClient::new(sock.clone()),
            );
            let span = tracing::info_span!("uffd.substrate_hello");
            let (canonical, session) = (args.canonical_manifest, args.session_manifest);
            tokio::task::spawn_blocking(move || {
                let probes = span.in_scope(|| {
                    client.hello(
                        Some(canonical),
                        Some(session),
                        engram_uffd_handler::FAULT_THREADS,
                    )
                });
                (client, probes)
            })
        });
        let blob = match args.blob_root.as_ref() {
            Some(root) => {
                tokio::fs::create_dir_all(root)
                    .await
                    .map_err(|e| format!("create --blob-root {}: {e}", root.display()))?;
                tracing::info!(path = %root.display(), "blob backend: local (overridden via --blob-root)");
                let s: Arc<dyn BlobStorage> =
                    Arc::new(engram_storage_local::LocalBlobStorage::new(root.clone()));
                s
            }
            None => {
                pick_blob_backend(&args.cache_root)
                    .instrument(tracing::info_span!("uffd.blob_backend"))
                    .await?
            }
        };
        tokio::fs::create_dir_all(&args.cache_root)
            .await
            .map_err(|e| format!("create cache root {}: {e}", args.cache_root.display()))?;

        // The session's published first-touch prefix, read beside the
        // startup work: the first-touch recorder carries it forward.
        let previous_prefix = args.trace_key.map(|session| {
            let blob = blob.clone();
            tokio::spawn(async move {
                let key = engram_chunk_store::FirstTouch::key(session);
                match blob.get(&key).await {
                    Ok(bytes) => engram_chunk_store::FirstTouch::decode_indices(&bytes),
                    Err(engram_core::BlobError::NotFound) => None,
                    Err(e) => {
                        tracing::warn!(error = %e, "first-touch prefix unreadable");
                        None
                    }
                }
            })
        });

        let populate = match hello {
            Some(hello) => {
                let (client, hello) = hello
                    .await
                    .map_err(|e| format!("substrate hello join: {e}"))?;
                match hello {
                    Ok((tmpfs_ok, cache_writable)) => {
                        if !tmpfs_ok {
                            return Err(
                                "substrate readiness: uffd base dir is NOT tmpfs — refusing to \
                                 serve (the guest would die later with `register memory … \
                                 userfaultfd … System error`)"
                                    .to_string(),
                            );
                        }
                        if !cache_writable {
                            tracing::warn!(
                                "substrate readiness: the writer's cache root is not writable",
                            );
                        }
                        Some(client)
                    }
                    Err(e) => {
                        // Hello failing at STARTUP means no writer is
                        // serving this socket — and handler spawns are
                        // host-agent-driven, so the writer is alive at
                        // spawn in production. Run fallback-only rather
                        // than paying the per-miss retry budget against
                        // a socket that will not appear (FC test suites
                        // and the bake/materialize profile pass hit this
                        // arm by design).
                        tracing::warn!(
                            error = %e,
                            "substrate Hello failed at startup; running fallback-only",
                        );
                        None
                    }
                }
            }
            None => None,
        };

        let (backend, canonical_fell_back) = ChunkedMemoryBackend::build_with_canonical_fallback(
            args.canonical_manifest,
            args.session_manifest,
            engram_uffd_handler::chunked::LocalManifests {
                canonical: args.canonical_manifest_json.as_deref(),
                session: args.session_manifest_json.as_deref(),
            },
            blob.clone(),
            &args.cache_root,
            populate,
        )
        .instrument(tracing::info_span!("uffd.build_backend"))
        .await
        .map_err(|e| format!("build chunked backend: {e}"))?;
        let backend = Arc::new(backend);

        // ADR 0045 C2: peer mode. Ordering is the soundness story:
        //   1. bind the control sock (host-agent can subscribe NOW);
        //   2. connect the peer session — BLOCKS until the source's
        //      capture registers the export and pushes the Seal (the
        //      page server parks pre-capture Hellos);
        //   3. only then bind the FC-facing UDS (inside run_listener).
        // FC's snapshot load waits on the UDS path, so "FC can touch
        // guest memory" structurally implies "seal held" — no fault is
        // ever served without a classification.
        let peer_wiring = match (args.peer_addr.as_ref(), args.peer_export_id.as_ref()) {
            (Some(addr), Some(export_id)) => {
                let control = match args.control_sock.as_ref() {
                    Some(path) => {
                        let tx = engram_uffd_handler::peer::ControlTx::bind(path)
                            .map_err(|e| format!("bind --control-sock {}: {e}", path.display()))?;
                        Some(Arc::new(tx))
                    }
                    None => None,
                };
                let token = args.peer_token.clone().expect("validated in parse_args");
                let addr = addr.clone();
                let export_id = export_id.clone();
                let chunk_size = backend.chunk_size();
                let total_bytes = backend.total_bytes();
                // The dial blocks (server parks until capture) — do it off
                // the async runtime.
                let session = tokio::task::spawn_blocking(move || {
                    engram_uffd_handler::peer::PeerSession::connect(
                        addr,
                        export_id,
                        token,
                        chunk_size,
                        total_bytes,
                    )
                })
                .instrument(tracing::info_span!("uffd.peer_connect"))
                .await
                .map_err(|e| format!("peer connect join: {e}"))?
                .map_err(|e| format!("peer connect: {e}"))?;
                let session = Arc::new(session);
                if let Some(control) = control.as_ref() {
                    control.report(engram_migrate_proto::HandlerControl::Sealed {
                        dirty_chunks: session.seal().count_ones(),
                        total_chunks: session.seal().chunk_count,
                        at_unix_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0),
                    });
                }
                Some((session, control))
            }
            _ => None,
        };

        // ADR 0045 substrate (v2b): create + size the base shm file NOW —
        // before run_listener binds the UDS. The host-agent orders FC's
        // load (which open(O_RDONLY)s + mmaps this file) after the socket
        // appears, so the file is always fully sized by then.
        // #1066: the base shm file is keyed and SIZED by the canonical
        // template. When the backend fell back to canonical == session,
        // pairing this session with the template's shm would be exactly
        // the layout mismatch the fallback just avoided — run without a
        // shared base instead (pre-D4 behavior; COPY-served pages).
        let base_shm_arg = if canonical_fell_back {
            None
        } else {
            args.base_shm.as_ref()
        };
        let base_shm = match base_shm_arg {
            Some(path) => {
                let b = tracing::info_span!("uffd.base_shm_open")
                    .in_scope(|| {
                        engram_uffd_handler::base_shm::BaseShm::open(path, backend.total_bytes())
                    })
                    .map_err(|e| format!("base shm: {e}"))?;
                tracing::info!(
                    path = %path.display(),
                    total_bytes = backend.total_bytes(),
                    "substrate base shm ready (canonical -> CONTINUE, divergent -> COPY)"
                );
                Some(b)
            }
            None => None,
        };
        if let Some(ack) = args.base_shm_ack.as_ref() {
            let opened = base_shm.as_ref().and(base_shm_arg);
            engram_uffd_handler::base_shm::write_ack(ack, opened.map(PathBuf::as_path))
                .map_err(|e| format!("write base shm ack {}: {e}", ack.display()))?;
        }

        tokio::task::spawn_blocking({
            let listen = args.listen.clone();
            move || {
                engram_uffd_handler::runtime::run_listener(
                    listen,
                    backend,
                    handle,
                    engram_uffd_handler::runtime::RunListenerOpts {
                        fault_around_chunks: args.fault_around_chunks,
                        migration_hint: args.migration_hint,
                        report_dir: args.report_dir,
                        base_shm,
                        peer: peer_wiring,
                        sealed_output: args.sealed_output,
                        previous_prefix,
                    },
                )
            }
        })
        .await
        .map_err(|e| format!("fault-loop join: {e}"))?
        .map_err(|e| format!("fault loop: {e}"))
    }

    /// `ENGRAM_BLOB_BACKEND` (default `local`) decides backend, via
    /// the one shared client (`engram_blob_client::from_env` — retry,
    /// deadlines, and metrics included; a dropped GET here would EIO
    /// a guest page-in). Local-mode blob root resolves from
    /// `ENGRAM_LOCAL_PATH/blobs`, the same place the host-agent's
    /// PooledBackend writes. `_cache_root` is the L1 NVMe cache
    /// (separate dir); reads first hit it, miss falls through to the
    /// blob root.
    async fn pick_blob_backend(_cache_root: &Path) -> Result<Arc<dyn BlobStorage>, String> {
        engram_blob_client::from_env().await
    }

    const HELP: &str = "engram-uffd-handler \\
  --listen <sock> \\
  --canonical-manifest <uuid>@v<num> \\
  --session-manifest <uuid>@v<num> \\
  [--fault-around-chunks N] (env ENGRAM_UFFD_FAULT_AROUND, default 8)
  [--trace-key <session-uuid>] [--report-dir <dir>] \\
  [--cache-root <path>] \\
  [--peer-addr <host:port> --peer-export-id <id> [--peer-token <tok>]] \\
  [--control-sock <sock>] [--sealed-output <path>] [--migration-hint <path>]

Peer mode (ADR 0045 C2, post-copy destination): with --peer-addr +
--peer-export-id (token via ENGRAM_PEER_TOKEN), the handler connects
to the SOURCE host-agent's page server, blocks for the sealed dirty
bitmap BEFORE binding <sock>, serves sealed faults from the peer
(sha-verified) and drains the rest in the background. --control-sock
streams Sealed/DrainProgress/DrainDone/PeerLost to the host-agent.
--sealed-output records the peer-supplied chunks for the first checkpoint.
--migration-hint names the source's hot-set hint, which orders the drain.

UFFD page-fault handler (ADR 0020 Route B). Firecracker connects to
<sock>, hands over the guest's UFFD, and the handler serves every
fault by resolving its offset against a chunked memory manifest and
copying the resolved chunk from the chunk cache/store (zero-filled
chunks via UFFDIO_ZEROPAGE). No memory.bin file is read.

ENGRAM_BLOB_BACKEND={local,gcs} (default local) picks the chunk
store's blob backend. ENGRAM_GCS_BUCKET is required when gcs.";
}
