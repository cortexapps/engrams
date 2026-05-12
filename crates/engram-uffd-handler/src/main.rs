//! `engram-uffd-handler` — page-fault handler binary.
//!
//! ADR 0007 chunked-memory shape. Per restore, FC connects to the
//! handler's UDS, hands over the userfaultfd + JSON mappings, and
//! the handler serves faults by resolving each fault offset against
//! a [`ChunkedMemoryBackend`]: pages identical to the canonical
//! base mmap serve from page cache; session-divergent pages fetch
//! the relevant chunk from the chunk store.
//!
//! ```text
//! engram-uffd-handler \
//!   --listen /tmp/uffd.sock \
//!   --canonical-memory /var/lib/engram/canonical/<image-id>.bin \
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

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

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

    let handle = rt.handle().clone();
    match rt.block_on(linux::run(args, handle)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("engram-uffd-handler: {e}");
            ExitCode::from(1)
        }
    }
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
        pub canonical_memory: PathBuf,
        pub canonical_manifest: ManifestRef,
        pub session_manifest: ManifestRef,
        pub prefault_trace: Option<PrefaultTraceSpec>,
        /// If `Some(host_id)`, publish the recorded trace as
        /// `traces/<session_manifest.manifest_id>/<host_id>.json` on
        /// clean shutdown. Omit on bake / unit-test runs where no
        /// publishing is wanted.
        pub publish_trace_host: Option<Uuid>,
        pub cache_root: PathBuf,
        pub cache_budget_bytes: u64,
        pub recorder_window: Duration,
    }

    pub fn parse_args() -> Result<Args, String> {
        let mut listen: Option<PathBuf> = None;
        let mut canonical_memory: Option<PathBuf> = None;
        let mut canonical_manifest: Option<ManifestRef> = None;
        let mut session_manifest: Option<ManifestRef> = None;
        let mut prefault_trace: Option<PrefaultTraceSpec> = None;
        let mut publish_trace_host: Option<Uuid> = None;
        let mut cache_root: Option<PathBuf> = None;
        let mut cache_budget_bytes: u64 = DEFAULT_CACHE_BUDGET_BYTES;
        let mut recorder_window_ms: u64 = DEFAULT_RECORDER_WINDOW_MS;

        let mut argv = std::env::args().skip(1);
        while let Some(arg) = argv.next() {
            match arg.as_str() {
                "--listen" => {
                    listen = Some(PathBuf::from(
                        argv.next()
                            .ok_or_else(|| "--listen requires a value".to_string())?,
                    ));
                }
                "--canonical-memory" => {
                    canonical_memory =
                        Some(PathBuf::from(argv.next().ok_or_else(|| {
                            "--canonical-memory requires a value".to_string()
                        })?));
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
        let canonical_memory =
            canonical_memory.ok_or_else(|| "--canonical-memory <path> is required".to_string())?;
        let canonical_manifest = canonical_manifest
            .ok_or_else(|| "--canonical-manifest <uuid>@v<n> is required".to_string())?;
        let session_manifest = session_manifest
            .ok_or_else(|| "--session-manifest <uuid>@v<n> is required".to_string())?;
        let cache_root = cache_root.unwrap_or_else(|| PathBuf::from("/var/cache/engram/chunks"));

        Ok(Args {
            listen,
            canonical_memory,
            canonical_manifest,
            session_manifest,
            prefault_trace,
            publish_trace_host,
            cache_root,
            cache_budget_bytes,
            recorder_window: Duration::from_millis(recorder_window_ms),
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
        let blob = pick_blob_backend(&args.cache_root).await?;
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

        let result = tokio::task::spawn_blocking({
            let handle = handle.clone();
            let listen = args.listen.clone();
            let canonical = args.canonical_memory.clone();
            let backend = backend.clone();
            let window = args.recorder_window;
            move || {
                engram_uffd_handler::runtime::run_listener(
                    listen, canonical, backend, handle, prefault, window,
                )
            }
        })
        .await
        .map_err(|e| format!("fault-loop join: {e}"))?;
        let trace = result.map_err(|e| format!("fault loop: {e}"))?;

        if let Some(host) = args.publish_trace_host {
            publish_trace(blob, args.session_manifest.manifest_id, host, &trace).await?;
        } else {
            tracing::info!(
                chunks = trace.chunks.len(),
                "fault loop exited; trace recorded but no --publish-trace-host set, dropping"
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
  --canonical-memory <path> \\
  --canonical-manifest <uuid>@v<num> \\
  --session-manifest <uuid>@v<num> \\
  [--prefault-trace canonical|<host-uuid>] \\
  [--publish-trace-host <host-uuid>] \\
  [--cache-root <path>] \\
  [--cache-budget-bytes <bytes>] \\
  [--recorder-window-ms <ms>]

UFFD page-fault handler. Firecracker connects to <sock>, hands over
the guest's UFFD, and the handler serves faults by resolving each
offset against a chunked memory manifest. Pages identical to the
canonical mmap serve from page cache; session-divergent pages fetch
the chunk from the chunk store.

ENGRAM_BLOB_BACKEND={local,gcs} (default local) picks the chunk
store's blob backend. ENGRAM_GCS_BUCKET is required when gcs.";
}
