//! `engram-uffd-handler` — page-fault handler binary.
//!
//! ADR 0020 Route B chunk-native shape. Per restore, FC connects to
//! the handler's UDS, hands over the userfaultfd + JSON mappings, and
//! the handler serves every fault by resolving its offset against a
//! [`ChunkedMemoryBackend`] and `UFFDIO_COPY`-ing the resolved chunk
//! from the chunk cache/store (zero-filled chunks via
//! `UFFDIO_ZEROPAGE`). There is no `memory.bin` file.
//!
//! ```text
//! engram-uffd-handler \
//!   --listen /tmp/uffd.sock \
//!   --canonical-manifest <uuid>@v<n> \
//!   --session-manifest   <uuid>@v<n> \
//!   [--prefault-trace canonical|<host-uuid>] \
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
    #[derive(Clone, Copy, Debug)]
    pub enum PrefaultTraceSpec {
        /// `traces/<manifest_id>/canonical.json` (image-bake-time).
        Canonical,
        /// `traces/<manifest_id>/<host_id>.json` (this host's prior
        /// recording for the same manifest).
        Host(Uuid),
    }

    pub struct Args {
        pub listen: PathBuf,
        pub canonical_manifest: ManifestRef,
        pub session_manifest: ManifestRef,
        pub prefault_trace: Option<PrefaultTraceSpec>,
        /// If `Some(host_id)`, publish the recorded trace as
        /// `traces/<session_manifest.manifest_id>/<host_id>.json` on
        /// clean shutdown. Omit on bake / unit-test runs where no
        /// publishing is wanted.
        pub publish_trace_host: Option<Uuid>,
        /// ADR 0014 M1.14: write the recorded trace JSON to this
        /// local path on clean shutdown (in addition to / instead of
        /// `--publish-trace-host`'s BlobStorage publish). Used by the
        /// image-builder's synthetic profile pass: the bake spawns
        /// FC+UFFD, dials vsock to drive activity, then reads the
        /// trace file back to stage it as an OCI layer. Omit when
        /// the receiver doesn't care.
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
    }

    pub fn parse_args() -> Result<Args, String> {
        let mut listen: Option<PathBuf> = None;
        let mut canonical_manifest: Option<ManifestRef> = None;
        let mut session_manifest: Option<ManifestRef> = None;
        let mut prefault_trace: Option<PrefaultTraceSpec> = None;
        let mut publish_trace_host: Option<Uuid> = None;
        let mut trace_output: Option<PathBuf> = None;
        let mut blob_root: Option<PathBuf> = None;
        let mut cache_root: Option<PathBuf> = None;
        let mut cache_budget_bytes: u64 = DEFAULT_CACHE_BUDGET_BYTES;
        let mut recorder_window_ms: u64 = DEFAULT_RECORDER_WINDOW_MS;
        let mut base_shm: Option<PathBuf> = None;

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

        Ok(Args {
            listen,
            canonical_manifest,
            session_manifest,
            prefault_trace,
            publish_trace_host,
            trace_output,
            blob_root,
            cache_root,
            cache_budget_bytes,
            recorder_window: Duration::from_millis(recorder_window_ms),
            base_shm,
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

    /// Accept `canonical` (string literal) or `<uuid>` (a host id).
    fn parse_trace_spec(s: &str) -> Result<PrefaultTraceSpec, String> {
        if s.eq_ignore_ascii_case("canonical") {
            Ok(PrefaultTraceSpec::Canonical)
        } else {
            let host = Uuid::parse_str(s).map_err(|e| {
                format!("--prefault-trace {s:?} (expected `canonical` or <uuid>): {e}")
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

        let backend = ChunkedMemoryBackend::from_blob(
            args.canonical_manifest,
            args.session_manifest,
            blob.clone(),
            &args.cache_root,
            args.cache_budget_bytes,
        )
        .await
        .map_err(|e| format!("build chunked backend: {e}"))?;
        let backend = Arc::new(backend);

        let prefault = if let Some(spec) = args.prefault_trace {
            let trace_ref = match spec {
                PrefaultTraceSpec::Canonical => {
                    TraceRef::canonical(args.session_manifest.manifest_id)
                }
                PrefaultTraceSpec::Host(host) => {
                    TraceRef::host(args.session_manifest.manifest_id, host)
                }
            };
            match load_trace_via_blob(blob.clone(), trace_ref).await {
                Ok(t) => {
                    tracing::info!(chunks = t.chunks.len(), ?trace_ref, "loaded prefault trace");
                    Some(t)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        ?trace_ref,
                        "prefault trace requested but couldn't be loaded; proceeding without"
                    );
                    None
                }
            }
        } else {
            None
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
                    listen, backend, handle, prefault, window, trace_out, base_shm,
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
  [--prefault-trace canonical|<host-uuid>] \\
  [--publish-trace-host <host-uuid>] \\
  [--cache-root <path>] \\
  [--cache-budget-bytes <bytes>] \\
  [--recorder-window-ms <ms>]

UFFD page-fault handler (ADR 0020 Route B). Firecracker connects to
<sock>, hands over the guest's UFFD, and the handler serves every
fault by resolving its offset against a chunked memory manifest and
copying the resolved chunk from the chunk cache/store (zero-filled
chunks via UFFDIO_ZEROPAGE). No memory.bin file is read.

ENGRAM_BLOB_BACKEND={local,gcs} (default local) picks the chunk
store's blob backend. ENGRAM_GCS_BUCKET is required when gcs.";
}
