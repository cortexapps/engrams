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
//!   [--prefault-trace file:<path>|<host-uuid>] \
//!   [--publish-trace-host <host-uuid>] \
//!   [--cache-root /var/cache/engram/chunks] \
//!   [--cache-budget-bytes 214748364800] \
//!   [--recorder-window-ms 5000]
//! ```
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
    use std::time::Duration;

    use engram_chunk_store::working_set::{TraceRef, WorkingSetTrace};
    use engram_chunk_store::ChunkStore;
    use engram_core::traits::BlobStorage;
    use engram_core::types::manifest::ManifestRef;
    use engram_uffd_handler::ChunkedMemoryBackend;
    use tokio::runtime::Handle as TokioHandle;
    use uuid::Uuid;

    /// Default LRU budget for the per-host NVMe cache. 200 GiB
    /// matches the ADR's "L1 host cache" sizing — production hosts
    /// allocate a dedicated nvme partition for this.
    const DEFAULT_CACHE_BUDGET_BYTES: u64 = 200 * 1024 * 1024 * 1024;

    /// Default trace-recorder window. The literature (FaaSnap, REAP)
    /// converges on ~5 s as the right point: long enough to capture
    /// the steady-state working set, short enough that publishing
    /// the trace doesn't measurably delay snapshot finalisation.
    const DEFAULT_RECORDER_WINDOW_MS: u64 = 5_000;

    /// Which trace to consume on restore.
    #[derive(Clone, Debug)]
    pub enum PrefaultTraceSpec {
        /// `traces/<manifest_id>/<host_id>.json` (this host's prior
        /// recording for the same manifest).
        Host(Uuid),
        /// A local trace JSON staged by the spawn (post-copy: the
        /// SOURCE's hot set — drives drain ordering + prefault).
        File(PathBuf),
    }

    pub struct Args {
        pub listen: PathBuf,
        pub canonical_manifest: ManifestRef,
        pub session_manifest: ManifestRef,
        /// ADR 0045 C1: read the SESSION manifest from this local JSON
        /// file instead of the blob store — a migration destination
        /// restores from a not-yet-durable manifest whose chunks the
        /// host-agent pre-pulled into the NVMe cache.
        pub session_manifest_json: Option<PathBuf>,
        pub prefault_trace: Option<PrefaultTraceSpec>,
        /// If `Some(host_id)`, publish the recorded trace as
        /// `traces/<session_manifest.manifest_id>/<host_id>.json` on
        /// clean shutdown. Omit on bake / unit-test runs where no
        /// publishing is wanted.
        pub publish_trace_host: Option<Uuid>,
        /// The per-jail trace file path: write the recorded trace JSON
        /// here on clean shutdown (in addition to / instead of
        /// `--publish-trace-host`'s BlobStorage publish). ADR 0045 C2
        /// reads it back to ship the migration `hot_chunks` rider. Omit
        /// when the receiver doesn't care.
        pub trace_output: Option<PathBuf>,
        /// ADR 0014 M1.14: when set, bypass `pick_blob_backend`'s
        /// env-var lookup and `LocalBlobStorage::new(this)` directly.
        /// Used by the image-builder's profile pass, where the bake
        /// already knows its chunk-store root and the UFFD handler
        /// needs to read from the same place — env-var-driven
        /// resolution would mis-locate it because the bake uses a
        /// `<images_dir>/store/` layout (not `<root>/blobs/`).
        pub blob_root: Option<PathBuf>,
        pub cache_root: PathBuf,
        pub cache_budget_bytes: u64,
        pub recorder_window: Duration,
        /// ADR 0045 substrate (v2b): the per-template base shm file this
        /// handler creates, sizes, and lazily populates with canonical
        /// chunks. The forked FC maps it MAP_PRIVATE and registers
        /// MISSING|MINOR; canonical faults resolve via UFFDIO_CONTINUE
        /// (shared), divergent via UFFDIO_COPY (private). Off when unset.
        pub base_shm: Option<PathBuf>,
        /// ADR 0045 C2 (post-copy destination): `host:port` of the SOURCE
        /// host-agent's page server. With `--peer-export-id`, arms peer
        /// mode: connect + authenticate + block for the `Seal` BEFORE
        /// binding the FC-facing UDS (so "FC can load" ⇒ "seal held"),
        /// then serve sealed faults from the peer and drain the rest in
        /// the background. The token rides `ENGRAM_PEER_TOKEN` (argv
        /// leaks via /proc/*/cmdline); `--peer-token` is a dev override.
        pub peer_addr: Option<String>,
        pub peer_export_id: Option<String>,
        pub peer_token: Option<String>,
        /// ADR 0045 C2: bind a UnixListener here and stream one-way
        /// `HandlerControl` reports (Sealed/DrainProgress/DrainDone/
        /// PeerLost) to whichever host-agent dials in.
        pub control_sock: Option<PathBuf>,
    }

    pub fn parse_args() -> Result<Args, String> {
        let mut listen: Option<PathBuf> = None;
        let mut canonical_manifest: Option<ManifestRef> = None;
        let mut session_manifest: Option<ManifestRef> = None;
        let mut session_manifest_json: Option<PathBuf> = None;
        let mut prefault_trace: Option<PrefaultTraceSpec> = None;
        let mut publish_trace_host: Option<Uuid> = None;
        let mut trace_output: Option<PathBuf> = None;
        let mut blob_root: Option<PathBuf> = None;
        let mut cache_root: Option<PathBuf> = None;
        let mut cache_budget_bytes: u64 = DEFAULT_CACHE_BUDGET_BYTES;
        let mut recorder_window_ms: u64 = DEFAULT_RECORDER_WINDOW_MS;
        let mut base_shm: Option<PathBuf> = None;
        let mut peer_addr: Option<String> = None;
        let mut peer_export_id: Option<String> = None;
        let mut peer_token: Option<String> = None;
        let mut control_sock: Option<PathBuf> = None;

        let mut argv = std::env::args().skip(1);
        while let Some(arg) = argv.next() {
            match arg.as_str() {
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
                "--session-manifest-json" => {
                    let v = argv
                        .next()
                        .ok_or_else(|| "--session-manifest-json requires a value".to_string())?;
                    session_manifest_json = Some(PathBuf::from(v));
                }
                "--prefault-trace" => {
                    let v = argv
                        .next()
                        .ok_or_else(|| "--prefault-trace requires a value".to_string())?;
                    prefault_trace = Some(parse_trace_spec(&v)?);
                }
                "--publish-trace-host" => {
                    let v = argv
                        .next()
                        .ok_or_else(|| "--publish-trace-host requires a value".to_string())?;
                    publish_trace_host = Some(
                        Uuid::parse_str(&v)
                            .map_err(|e| format!("--publish-trace-host {v:?}: {e}"))?,
                    );
                }
                "--trace-output" => {
                    trace_output =
                        Some(PathBuf::from(argv.next().ok_or_else(|| {
                            "--trace-output requires a value".to_string()
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
                "--cache-budget-bytes" => {
                    let v = argv
                        .next()
                        .ok_or_else(|| "--cache-budget-bytes requires a value".to_string())?;
                    cache_budget_bytes = v
                        .parse::<u64>()
                        .map_err(|e| format!("--cache-budget-bytes {v:?}: {e}"))?;
                }
                "--base-shm" => {
                    base_shm = Some(PathBuf::from(
                        argv.next()
                            .ok_or_else(|| "--base-shm requires a value".to_string())?,
                    ));
                }
                "--recorder-window-ms" => {
                    let v = argv
                        .next()
                        .ok_or_else(|| "--recorder-window-ms requires a value".to_string())?;
                    recorder_window_ms = v
                        .parse::<u64>()
                        .map_err(|e| format!("--recorder-window-ms {v:?}: {e}"))?;
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

        Ok(Args {
            listen,
            canonical_manifest,
            session_manifest,
            session_manifest_json,
            prefault_trace,
            publish_trace_host,
            trace_output,
            blob_root,
            cache_root,
            cache_budget_bytes,
            recorder_window: Duration::from_millis(recorder_window_ms),
            base_shm,
            peer_addr,
            peer_export_id,
            peer_token,
            control_sock,
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

    /// Accept `file:<path>` (a local trace JSON) or `<uuid>` (a host id).
    fn parse_trace_spec(s: &str) -> Result<PrefaultTraceSpec, String> {
        if let Some(path) = s.strip_prefix("file:") {
            Ok(PrefaultTraceSpec::File(PathBuf::from(path)))
        } else {
            let host = Uuid::parse_str(s).map_err(|e| {
                format!("--prefault-trace {s:?} (expected `file:<path>` or <uuid>): {e}")
            })?;
            Ok(PrefaultTraceSpec::Host(host))
        }
    }

    pub async fn run(args: Args, handle: TokioHandle) -> Result<(), String> {
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
            None => pick_blob_backend(&args.cache_root).await?,
        };
        tokio::fs::create_dir_all(&args.cache_root)
            .await
            .map_err(|e| format!("create cache root {}: {e}", args.cache_root.display()))?;

        let backend = ChunkedMemoryBackend::from_blob_with_session_json(
            args.canonical_manifest,
            args.session_manifest,
            args.session_manifest_json.as_deref(),
            blob.clone(),
            &args.cache_root,
            args.cache_budget_bytes,
        )
        .await
        .map_err(|e| format!("build chunked backend: {e}"))?;
        let backend = Arc::new(backend);

        let prefault = if let Some(spec) = args.prefault_trace {
            let loaded = match &spec {
                PrefaultTraceSpec::File(path) => tokio::fs::read(path)
                    .await
                    .map_err(|e| format!("read {}: {e}", path.display()))
                    .and_then(|bytes| {
                        serde_json::from_slice(&bytes).map_err(|e| format!("parse trace: {e}"))
                    }),
                PrefaultTraceSpec::Host(host) => load_trace_via_blob(
                    blob.clone(),
                    TraceRef::host(args.session_manifest.manifest_id, *host),
                )
                .await
                .map_err(|e| e.to_string()),
            };
            match loaded {
                Ok(t) => {
                    tracing::info!(chunks = t.chunks.len(), ?spec, "loaded prefault trace");
                    Some(t)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        ?spec,
                        "prefault trace requested but couldn't be loaded; proceeding without"
                    );
                    None
                }
            }
        } else {
            None
        };

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
        let base_shm = match args.base_shm.as_ref() {
            Some(path) => {
                let b = engram_uffd_handler::base_shm::BaseShm::open(path, backend.total_bytes())
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

        let result = tokio::task::spawn_blocking({
            let handle = handle.clone();
            let listen = args.listen.clone();
            let backend = backend.clone();
            let window = args.recorder_window;
            let trace_out = args.trace_output.clone();
            move || {
                engram_uffd_handler::runtime::run_listener(
                    listen,
                    backend,
                    handle,
                    engram_uffd_handler::runtime::RunListenerOpts {
                        prefault_trace: prefault,
                        recorder_window: window,
                        trace_output: trace_out,
                        base_shm,
                        peer: peer_wiring,
                    },
                )
            }
        })
        .await
        .map_err(|e| format!("fault-loop join: {e}"))?;
        let trace = result.map_err(|e| format!("fault loop: {e}"))?;

        // ADR 0014 M1.14: dump the trace to a local file first, so a
        // later publish_trace failure (e.g., the bake's blob root
        // has a different layout than runtime's) doesn't lose the
        // recorded data. The image-builder's profile pass reads
        // this file back to stage the trace as an OCI layer.
        if let Some(path) = args.trace_output.as_ref() {
            let bytes = serde_json::to_vec(&trace)
                .map_err(|e| format!("serialize trace for --trace-output: {e}"))?;
            tokio::fs::write(path, &bytes)
                .await
                .map_err(|e| format!("write trace to {}: {e}", path.display()))?;
            tracing::info!(
                chunks = trace.chunks.len(),
                path = %path.display(),
                "trace written to --trace-output"
            );
        }

        if let Some(host) = args.publish_trace_host {
            // Best-effort publish — log and continue rather than
            // propagating, so the --trace-output side effect above
            // sticks even when BlobStorage isn't reachable.
            if let Err(e) =
                publish_trace(blob, args.session_manifest.manifest_id, host, &trace).await
            {
                tracing::warn!(error = %e, "publish_trace failed; trace_output still written");
            }
        } else if args.trace_output.is_none() {
            tracing::info!(
                chunks = trace.chunks.len(),
                "fault loop exited; trace recorded but no publish target set, dropping"
            );
        }

        Ok(())
    }

    /// `ENGRAM_BLOB_BACKEND` (default `local`) decides backend.
    /// Mirrors the convention every other Engram crate uses (see
    /// `engram_image_builder::blob`, `engram_coordinator::blob`,
    /// `engram_host_agent::blob`).
    ///
    /// Local-mode blob root resolves from `ENGRAM_LOCAL_PATH/blobs`
    /// to match `engram_host_agent::blob::from_env` exactly — that
    /// way the chunked-memory bytes the host-agent's PooledBackend
    /// just wrote land at the same place this handler reads them
    /// from. `_cache_root` is the L1 NVMe cache (separate dir);
    /// reads first hit it, miss falls through to the blob root.
    async fn pick_blob_backend(_cache_root: &Path) -> Result<Arc<dyn BlobStorage>, String> {
        let backend = std::env::var("ENGRAM_BLOB_BACKEND")
            .unwrap_or_else(|_| "local".to_string())
            .to_lowercase();
        match backend.as_str() {
            "local" => {
                let root = std::env::var("ENGRAM_LOCAL_PATH")
                    .unwrap_or_else(|_| "./var/engram".to_string());
                let blobs_dir = std::path::PathBuf::from(root).join("blobs");
                tokio::fs::create_dir_all(&blobs_dir)
                    .await
                    .map_err(|e| format!("create local blob root {}: {e}", blobs_dir.display()))?;
                tracing::info!(path = %blobs_dir.display(), "blob backend: local");
                Ok(Arc::new(engram_storage_local::LocalBlobStorage::new(
                    blobs_dir,
                )))
            }
            "gcs" => {
                let bucket = std::env::var("ENGRAM_GCS_BUCKET").map_err(|_| {
                    "ENGRAM_BLOB_BACKEND=gcs requires ENGRAM_GCS_BUCKET".to_string()
                })?;
                tracing::info!(bucket = %bucket, "blob backend: gcs");
                let store = engram_storage_gcs::GcsBlobStorage::connect(bucket)
                    .await
                    .map_err(|e| format!("gcs connect: {e}"))?;
                Ok(Arc::new(store))
            }
            other => Err(format!(
                "unknown ENGRAM_BLOB_BACKEND={other}; expected `local` or `gcs`"
            )),
        }
    }

    async fn load_trace_via_blob(
        blob: Arc<dyn BlobStorage>,
        trace_ref: TraceRef,
    ) -> Result<WorkingSetTrace, String> {
        let store = ChunkStore::new(blob);
        store
            .get_trace(trace_ref)
            .await
            .map_err(|e| format!("get_trace: {e}"))?
            .ok_or_else(|| format!("trace {trace_ref:?} not found"))
    }

    async fn publish_trace(
        blob: Arc<dyn BlobStorage>,
        manifest_id: Uuid,
        host_id: Uuid,
        trace: &WorkingSetTrace,
    ) -> Result<(), String> {
        let store = ChunkStore::new(blob);
        let trace_ref = TraceRef::host(manifest_id, host_id);
        store
            .put_trace(trace_ref, trace)
            .await
            .map_err(|e| format!("put_trace {trace_ref:?}: {e}"))?;
        tracing::info!(
            chunks = trace.chunks.len(),
            ?trace_ref,
            "published working-set trace"
        );
        Ok(())
    }

    const HELP: &str = "engram-uffd-handler \\
  --listen <sock> \\
  --canonical-manifest <uuid>@v<num> \\
  --session-manifest <uuid>@v<num> \\
  [--prefault-trace file:<path>|<host-uuid>] \\
  [--publish-trace-host <host-uuid>] \\
  [--cache-root <path>] \\
  [--cache-budget-bytes <bytes>] \\
  [--recorder-window-ms <ms>] \\
  [--peer-addr <host:port> --peer-export-id <id> [--peer-token <tok>]] \\
  [--control-sock <sock>]

Peer mode (ADR 0045 C2, post-copy destination): with --peer-addr +
--peer-export-id (token via ENGRAM_PEER_TOKEN), the handler connects
to the SOURCE host-agent's page server, blocks for the sealed dirty
bitmap BEFORE binding <sock>, serves sealed faults from the peer
(sha-verified) and drains the rest in the background. --control-sock
streams Sealed/DrainProgress/DrainDone/PeerLost to the host-agent.

UFFD page-fault handler (ADR 0020 Route B). Firecracker connects to
<sock>, hands over the guest's UFFD, and the handler serves every
fault by resolving its offset against a chunked memory manifest and
copying the resolved chunk from the chunk cache/store (zero-filled
chunks via UFFDIO_ZEROPAGE). No memory.bin file is read.

ENGRAM_BLOB_BACKEND={local,gcs} (default local) picks the chunk
store's blob backend. ENGRAM_GCS_BUCKET is required when gcs.";
}
