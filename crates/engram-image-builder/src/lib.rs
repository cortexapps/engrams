//! Image baker.
//!
//! ADR 0080: the source of truth for an image is its **Dockerfile** —
//! the bake carries no runtime config. An optional `engram.toml` holds
//! only the `[build]` section (Dockerfile path, context, args, build
//! secrets). Everything the runtime needs (name/description/env/workdir/
//! resources/warm) is supplied out-of-band at enable time via
//! `engram image enable --config`; the artifact's config blob carries the
//! Dockerfile-derived `runtime_defaults` (ENV/WORKDIR) so the platform
//! never re-reads the OCI image config after enable.
//!
//! `engram-image-builder build` runs `docker build`, exports the
//! resulting OCI image's filesystem to the registry layout the
//! coordinator reads:
//!
//! ```text
//!   <images_dir>/<repo>/<tag>/
//!     rootfs/             # ProcessBackend dev path
//!     rootfs.ext4         # FirecrackerBackend prod path
//!     bundle.json         # chunk-manifest pointer (Ext4 bakes)
//! ```
//!
//! For ProcessBackend dev, we extract via `docker create + docker
//! export | tar -x`. For Firecracker prod, the same pipe continues into
//! `mkfs.ext4` (the only injected file is the stage-1 init shim).

pub mod blob;
pub mod config;
pub mod docker;

use std::path::{Path, PathBuf};

use engram_core::types::image::OciRuntimeDefaults;
use engram_rootfs_materializer::{inject_init, recursive_size};

pub use config::{BuildConfig, EngramRepoConfig};
pub use docker::{DockerCli, DockerRunner};
// ADR 0080: the ext4 packer and the stage-1 init shim moved to
// `engram-rootfs-materializer` — ONE home for tree→ext4 packing and
// ONE shim source, shared between this (retiring) docker-export bake
// and the enable-time materializer. Re-exported so consumers keep
// their `engram_image_builder::{ext4::…, InitInjection, Transport}`
// paths until the phase-4 retirement.
pub use engram_rootfs_materializer::ext4;
pub use engram_rootfs_materializer::{
    recommended_size, Ext4Error, Ext4Packer, InitInjection, Mke2fsPacker, Transport,
};

/// Output format the baker should produce.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Format {
    /// `<image_dir>/rootfs/` — directory tree. Consumed by
    /// `ProcessBackend` (the dev backend on macOS / non-KVM Linux).
    #[default]
    Directory,
    /// `<image_dir>/rootfs.ext4` — ext4 filesystem image. Consumed by
    /// `FirecrackerBackend` as a block device.
    Ext4,
}

/// What the user wants the baker to do.
#[derive(Clone, Debug)]
pub struct BuildRequest {
    /// Path to the source repo (containing `Dockerfile` and `engram.toml`).
    pub source: PathBuf,
    /// Repo identifier in the registry — typically `<org>/<name>`.
    pub repo: String,
    /// Tag for the produced image. Convention: `warm-<rfc3339>` so the
    /// coordinator's "latest ready" lookup picks it up by created_at.
    pub tag: String,
    /// Where the registry lives (the same root the coordinator reads).
    pub images_dir: PathBuf,
    /// Output format — `Directory` for the dev backend, `Ext4` for
    /// Firecracker. Defaults to `Directory` so existing callers keep
    /// working.
    pub format: Format,
    /// Optional: write the ADR 0080 stage-1 init shim to
    /// `/sbin/engram-init`. The shim mounts the essentials, mounts the
    /// aux bundle slots, copies `engram-agentd` out of its reserved
    /// bundle slot to tmpfs, and exec's the copy on a vsock port —
    /// agentd itself is NEVER baked into the rootfs. Pair with FC's
    /// `default_boot_args = "... init=/sbin/engram-init"` and a host
    /// that stages `bundle-agentd`.
    pub init_injection: Option<InitInjection>,
}

#[derive(Clone, Debug)]
pub struct BuildOutcome {
    pub image_dir: PathBuf,
    /// ADR 0080: the built image's Dockerfile `ENV` + `WORKDIR`, read via
    /// `docker inspect` at bake time. `push_to_registry` embeds this in
    /// the artifact's config blob (`runtime_defaults`); the coordinator
    /// persists it at enable so session-create merges it under the
    /// RPC-supplied `ImageConfig` with no OCI re-read.
    pub runtime_defaults: OciRuntimeDefaults,
    /// Path on disk where the rootfs lives (directory for
    /// `Format::Directory`; `.ext4` file for `Format::Ext4`).
    /// Kept for ProcessBackend dev path; production consumers
    /// resolve content via `disk_manifest` instead.
    pub rootfs_path: PathBuf,
    /// ADR 0007: content-addressed chunked manifest pointing at
    /// the disk's bytes in `BlobStorage`. `None` for
    /// `Format::Directory` bakes (ProcessBackend reads the
    /// rootfs as files); `Some` for `Format::Ext4` bakes. The
    /// host-agent's image cache adopts this ref to materialize
    /// per-sandbox disks via NBD (Linux+FC) or materialize-to-
    /// file (macOS+VZ).
    pub disk_manifest: Option<engram_chunk_store::ManifestRef>,
    /// Size of `rootfs_path` on disk.
    pub size_bytes: u64,
    /// ADR 0008 Phase 3 / ADR 0036: per-chunk bootstrap JSON for
    /// the disk side. Sits next to `rootfs.ext4` in `image_dir`
    /// (e.g. `image_dir/bootstrap.disk.json`). `None` for non-Ext4
    /// bakes (Directory) or when chunked-OCI emission was skipped.
    /// `push_to_registry` reads it to drive the per-chunk delta
    /// push (chunk bytes come from the bake's chunk store).
    pub disk_bootstrap_path: Option<PathBuf>,
}

#[derive(Debug)]
pub enum BuildError {
    Config(String),
    Io(std::io::Error),
    Docker(String),
    Ext4(Ext4Error),
    InvalidPath(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(m) => write!(f, "engram.toml: {m}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Docker(m) => write!(f, "docker: {m}"),
            Self::Ext4(e) => write!(f, "ext4: {e}"),
            Self::InvalidPath(p) => write!(f, "invalid path: {p}"),
        }
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Ext4(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for BuildError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<Ext4Error> for BuildError {
    fn from(e: Ext4Error) -> Self {
        Self::Ext4(e)
    }
}

impl From<engram_chunk_store::ChunkStoreError> for BuildError {
    fn from(e: engram_chunk_store::ChunkStoreError) -> Self {
        // Chunk-store errors surface as generic Config since
        // they're a fail-the-bake condition not categorically
        // different from any other I/O.
        Self::Config(format!("chunk store: {e}"))
    }
}

/// The baker. Composed of:
///
/// - A [`DockerRunner`] (mockable) for `docker build/create/export`.
/// - An [`Ext4Packer`] (mockable) for the `Format::Ext4` step.
/// - A [`engram_chunk_store::ChunkStore`] that the ext4 path
///   chunks the produced rootfs into (ADR 0007). The store can
///   be backed by `LocalBlobStorage` in dev or
///   `GcsBlobStorage` / `S3BlobStorage` in production CI.
///
/// Stage A decoupled bake from Postgres; baked artifacts are
/// pushed to the registry via [`Self::push_image`] and the
/// coordinator's `/api/enabled-images` POST handler enables them
/// — the builder never touches a `MetadataStore`.
pub struct Builder<D: DockerRunner, P: Ext4Packer = Mke2fsPacker> {
    docker: D,
    packer: P,
    chunk_store: engram_chunk_store::ChunkStore,
    /// OCI client for registry pushes. Optional so tests that exercise
    /// only the local bake pipeline don't have to construct one; absence
    /// is surfaced as a clear error when a push actually needs it.
    oci: Option<engram_oci::OciClient>,
}

impl<D: DockerRunner> Builder<D, Mke2fsPacker> {
    /// Default constructor: real `mke2fs` packer for `Format::Ext4`,
    /// caller-supplied chunk store. Call [`Self::with_oci`] afterwards
    /// to attach the OCI client needed for registry pushes.
    pub fn new(docker: D, chunk_store: engram_chunk_store::ChunkStore) -> Self {
        Self {
            docker,
            packer: Mke2fsPacker::default(),
            chunk_store,
            oci: None,
        }
    }
}

impl<D: DockerRunner, P: Ext4Packer> Builder<D, P> {
    /// Construct with a custom packer — used by tests to record /
    /// inject errors instead of running real mke2fs.
    pub fn with_packer(docker: D, packer: P, chunk_store: engram_chunk_store::ChunkStore) -> Self {
        Self {
            docker,
            packer,
            chunk_store,
            oci: None,
        }
    }

    /// Attach the OCI client. Required for [`Self::push_to_registry`]
    /// (image push). Returns `self` so it composes with
    /// `Builder::new(...).with_oci(...)`.
    pub fn with_oci(mut self, oci: engram_oci::OciClient) -> Self {
        self.oci = Some(oci);
        self
    }

    /// Run a single bake. Steps:
    ///
    /// 1. Validate paths in `req`.
    /// 2. Read `<source>/engram.toml` (optional; `[build]` only — ADR 0080).
    /// 3. `docker build` against the source.
    /// 4. `docker create` a throwaway container; `docker export` its
    ///    filesystem to the staging tarball.
    /// 5. Extract the tarball into `<images_dir>/<repo>/<tag>/rootfs/`.
    /// 6. Extract the Dockerfile ENV/WORKDIR as `runtime_defaults`.
    /// 7. Best-effort `docker rm <container>` + remove the temporary tag.
    ///
    /// Idempotent on the registry side — overwrites any existing
    /// `<repo>/<tag>` directory. Postgres recording is delegated to
    /// [`Self::record_in_metadata`].
    pub async fn build(&self, req: &BuildRequest) -> Result<BuildOutcome, BuildError> {
        // 1. Path safety — reject `..` segments in repo / tag so a
        //    typo can't escape the registry root.
        for component in [req.repo.as_str(), req.tag.as_str()] {
            if component.is_empty() || component.contains("..") || component.starts_with('/') {
                return Err(BuildError::InvalidPath(component.to_string()));
            }
        }
        let source = req.source.canonicalize().map_err(|e| {
            BuildError::InvalidPath(format!("source {}: {e}", req.source.display()))
        })?;
        let image_dir = req.images_dir.join(&req.repo).join(&req.tag);

        // 2. Read engram.toml.
        let cfg_path = source.join("engram.toml");
        let cfg = EngramRepoConfig::read_from(&cfg_path)?;

        // 3. Pick a unique throwaway tag for the docker build artefact
        //    so concurrent bakes don't trample each other.
        let docker_tag = format!("engram-bake-{}", uuid::Uuid::new_v4().simple());
        let dockerfile = source.join(&cfg.build.dockerfile);
        if !dockerfile.exists() {
            return Err(BuildError::Config(format!(
                "Dockerfile not found at {}",
                dockerfile.display()
            )));
        }
        let context = source.join(&cfg.build.context);

        tracing::info!(repo = %req.repo, tag = %req.tag, ?dockerfile, "running docker build");
        self.docker
            .build(docker::BuildArgs {
                context: context.clone(),
                dockerfile: dockerfile.clone(),
                tag: docker_tag.clone(),
                build_args: cfg.build.args.clone(),
                // `[build] build_secrets` → BuildKit `--secret id=,env=`;
                // values come from the bake process env, never a layer.
                build_secrets: cfg.build.build_secrets.clone(),
            })
            .await
            .map_err(|e| BuildError::Docker(format!("build: {e}")))?;

        // 4. Create container. If create fails the only artefact is
        //    the build-tagged image, so just rmi that and bail.
        let container_id = match self.docker.create(&docker_tag).await {
            Ok(id) => id,
            Err(e) => {
                let _ = self.docker.rmi(&docker_tag).await;
                return Err(BuildError::Docker(format!("create: {e}")));
            }
        };

        // 5. Everything from here owns both the container and the
        //    image — guarantee cleanup runs whether export succeeds
        //    or fails. We don't surface cleanup errors; the bake's
        //    outcome is what matters.
        let outcome = self
            .build_after_container(req, &image_dir, &container_id, &docker_tag)
            .await;
        // Cleanup: `build_after_container` already frees the image+container
        // on its success path (after export, before the ext4 pack — see
        // there). This trailing pass covers the error paths and is an
        // idempotent no-op on success.
        let _ = self.docker.rm_container(&container_id).await;
        let _ = self.docker.rmi(&docker_tag).await;

        outcome.inspect(|o| {
            tracing::info!(
                repo = %req.repo,
                tag = %req.tag,
                size_bytes = o.size_bytes,
                "bake complete",
            );
        })
    }

    /// Steps 5–6 of `build`: export, extract the runtime defaults,
    /// optionally pack to ext4, compute size. Factored out so the
    /// unconditional cleanup at the bottom of `build` is symmetrical
    /// regardless of where this fails.
    async fn build_after_container(
        &self,
        req: &BuildRequest,
        image_dir: &Path,
        container_id: &str,
        docker_tag: &str,
    ) -> Result<BuildOutcome, BuildError> {
        tokio::fs::create_dir_all(image_dir).await?;
        let rootfs_dir = image_dir.join("rootfs");
        // Wipe any prior contents — bake is overwrite-semantics.
        let _ = tokio::fs::remove_dir_all(&rootfs_dir).await;
        let _ = tokio::fs::remove_file(image_dir.join("rootfs.ext4")).await;
        tokio::fs::create_dir_all(&rootfs_dir).await?;

        self.docker
            .export_to_dir(container_id, &rootfs_dir)
            .await
            .map_err(|e| BuildError::Docker(format!("export: {e}")))?;

        // Optional: inject the stage-1 init shim before we pack to ext4.
        // Done after docker export so the rootfs the user described in
        // their Dockerfile is the base; we just overlay the shim on top.
        //
        // ADR 0062: the image bakes NO harness. ADR 0080: it bakes NO
        // agentd either — both ride reserved bundle slots so they iterate
        // with zero re-bakes. The one engrams file in the rootfs is the
        // stage-1 init shim below (mounts the slots, copies agentd to
        // tmpfs, execs it).
        if let Some(injection) = &req.init_injection {
            inject_init(&rootfs_dir, injection).await?;
        }

        // ADR 0027: the share-file skill + the git forge glue is no
        // longer baked into the rootfs. It now lives in the fleet-wide
        // `skills` RO bundle the FC host mounts, and agentd activates the
        // right subset per session at SpawnHarness (gated on the forge
        // token) — see `engram-session-bundles`. This retires the old
        // `inject_share_helpers` / `inject_forge_helpers` bake step so a
        // skill edit ships fleet-wide by rolling the bundle, no re-bake.
        // The wrapper scripts + SKILL.md now live in `deploy/bundles/skills/`.

        // ADR 0080: extract the built image's Docker config (its `ENV` +
        // `WORKDIR`) as the artifact's `runtime_defaults`. This is the ONLY
        // place the platform reads the OCI image config — the coordinator
        // persists it at enable and merges it UNDER the RPC-supplied
        // ImageConfig at session create. Inspect the created-but-unstarted
        // container, whose `Config` mirrors the image's Env/WorkingDir.
        // HARD error: with no manifest.toml carrying a fold-in anymore, a
        // failed inspect would silently strip the Dockerfile ENV/WORKDIR
        // from every session of this image — fail the bake instead.
        let runtime_defaults = match self.docker.inspect_config(container_id).await {
            Ok(cfg) => OciRuntimeDefaults::from_docker_config(&cfg.env, cfg.working_dir.as_deref()),
            Err(e) => {
                return Err(BuildError::Docker(format!(
                    "docker inspect for the image config failed: {e}. The artifact's \
                     runtime_defaults (Dockerfile ENV/WORKDIR) can't be extracted, so the \
                     bake is aborted rather than shipping an image that silently drops them."
                )));
            }
        };

        // Free the docker image + container NOW. The rootfs is fully
        // exported to `rootfs_dir` and the last image read (inspect_config
        // above) is done, so nothing below — recursive_size, the ext4
        // pack, chunking — needs them. This is load-bearing for large warm
        // images: otherwise the ext4 pack runs with the image's layers
        // (~1x), the exported tree (~1x), AND the ext4 (~1x) all on disk at
        // once (~3x the image size), which overruns a CI runner with
        // `mke2fs: No space left on device`. Freeing here drops the pack's
        // peak to ~2x (tree + ext4). The caller's trailing cleanup is then
        // an idempotent no-op (and still covers the pre-export error paths).
        let _ = self.docker.rm_container(container_id).await;
        let _ = self.docker.rmi(docker_tag).await;
        // `rmi` drops the image reference but NOT the BuildKit cache, which
        // holds a full ~image-sized copy of the just-built layers. For a large
        // warm image (e.g. dev-brain: ~33 GiB tree → ~67 GiB ext4) that cache
        // lingers on disk through the pack below — tree + ext4 + cache then
        // overruns the CI runner with `mke2fs: No space left on device`.
        // Prune it now so the pack's peak is just (exported tree + ext4).
        // Best-effort: a prune failure must not fail the bake.
        if let Err(e) = self.docker.builder_prune().await {
            tracing::warn!(error = %e, "docker builder prune failed (non-fatal)");
        }

        let dir_size = recursive_size(&rootfs_dir).await?;

        let (rootfs_path, total_size, disk_manifest) = match req.format {
            Format::Directory => (rootfs_dir, dir_size, None),
            Format::Ext4 => {
                // Format the staging dir into a single ext4 image,
                // then drop the staging dir — Firecracker only needs
                // the .ext4. Doing the wipe AFTER pack succeeds means
                // a failed mke2fs leaves the directory in place for
                // diagnostics.
                let ext4_path = image_dir.join("rootfs.ext4");
                let ext4_size = recommended_size(dir_size);
                tracing::info!(
                    dir_size_bytes = dir_size,
                    dir_size_gib = dir_size as f64 / (1024.0 * 1024.0 * 1024.0),
                    ext4_size_bytes = ext4_size,
                    ext4_size_gib = ext4_size as f64 / (1024.0 * 1024.0 * 1024.0),
                    "sizing ext4 image from source-tree disk usage (du-style)"
                );
                self.packer.pack(&rootfs_dir, &ext4_path, ext4_size).await?;
                let _ = tokio::fs::remove_dir_all(&rootfs_dir).await;
                let on_disk = tokio::fs::metadata(&ext4_path).await?.len();

                // ADR 0007: chunk the ext4 into the content-addressed
                // store + commit a manifest. Production consumers
                // (FC's NBD daemon, VZ's materialize-to-file) read
                // from the manifest, not the ext4 file directly.
                // We keep the .ext4 around for now so dev workflows
                // and the OCI push path (which still uploads the
                // ext4 as a layer) don't break; Phase 6 retires the
                // file when SandboxBackend takes ManifestRef
                // natively.
                let m = self
                    .chunk_store
                    .chunk_file(
                        &ext4_path,
                        engram_chunk_store::ManifestKind::Disk,
                        None, // default 16 MiB
                    )
                    .await?;
                // ADR 0036 P4: the bake's manifest identity is derived
                // from its content, not minted at random. Deterministic
                // re-bakes of unchanged content therefore reproduce the
                // SAME ManifestRef — bundle.json stays byte-identical,
                // and the enable pipeline can recognize "already
                // captured this exact rootfs" and reuse the base
                // snapshot. Content-derived also means the same ref ⇒
                // the same manifest bytes, so an already-present
                // manifest (a prior identical bake) is success, not a
                // VersionConflict.
                let manifest_ref = m.content_ref();
                match self.chunk_store.get_manifest(manifest_ref).await {
                    Ok(_) => {
                        tracing::debug!(
                            manifest = %manifest_ref,
                            "content-identical manifest already in store; skipping put"
                        );
                    }
                    Err(_) => self.chunk_store.put_manifest(manifest_ref, &m).await?,
                }

                // Sidecar bundle.json so image_cache + tooling can
                // find the manifest ref without hitting Postgres.
                // Format is intentionally tiny so a future "list
                // images" CLI can fetch it cheaply.
                //
                let bundle = serde_json::json!({
                    "schema_version": 1,
                    "disk_manifest": manifest_ref,
                });
                tokio::fs::write(
                    image_dir.join("bundle.json"),
                    serde_json::to_vec_pretty(&bundle)
                        .map_err(|e| BuildError::Config(format!("bundle.json: {e}")))?,
                )
                .await?;

                tracing::info!(
                    repo = %req.repo,
                    tag = %req.tag,
                    manifest = %manifest_ref,
                    chunks = m.chunks.len(),
                    total_bytes = m.total_bytes,
                    "chunked ext4 into manifest"
                );

                (ext4_path, on_disk, Some(manifest_ref))
            }
        };

        // ADR 0008 Phase 3 / ADR 0036: produce the per-chunk
        // bootstrap alongside the existing bundle.json. Pure
        // metadata — every entry addresses its chunk by the chunk's
        // own sha256 (also its OCI blob digest and its BlobStorage
        // key), so no concatenated chunk blob is built or written.
        // `push_to_registry` pushes each chunk the registry is
        // missing as its own blob; the runtime resolver indexes the
        // bootstrap at pull time.
        let mut disk_bootstrap_path = None;
        if let Some(disk_ref) = disk_manifest {
            let manifest = self
                .chunk_store
                .get_manifest(disk_ref)
                .await
                .map_err(|e| BuildError::Config(format!("re-read disk manifest: {e}")))?;
            let bootstrap = engram_chunk_store::Bootstrap::build_per_chunk(&manifest);
            let bs_path = image_dir.join("bootstrap.disk.json");
            tokio::fs::write(
                &bs_path,
                serde_json::to_vec(&bootstrap)
                    .map_err(|e| BuildError::Config(format!("disk bootstrap json: {e}")))?,
            )
            .await?;
            disk_bootstrap_path = Some(bs_path);
        }

        // Re-write bundle.json with the disk_manifest now that the
        // ext4 branch (above) wrote a baseline. Schema bumps to v2
        // when the chunked-OCI bootstrap was produced — v1 readers
        // still see `disk_manifest` and work; v2 readers also
        // consult `bootstrap_disk_available` for chunk-on-fault.
        if let Some(disk_ref) = disk_manifest {
            let schema_version = if disk_bootstrap_path.is_some() { 2 } else { 1 };
            let mut bundle = serde_json::json!({
                "schema_version": schema_version,
                "disk_manifest": disk_ref,
            });
            if disk_bootstrap_path.is_some() {
                bundle["bootstrap_disk_available"] = serde_json::Value::Bool(true);
            }
            tokio::fs::write(
                image_dir.join("bundle.json"),
                serde_json::to_vec_pretty(&bundle)
                    .map_err(|e| BuildError::Config(format!("bundle.json: {e}")))?,
            )
            .await?;
        }

        Ok(BuildOutcome {
            image_dir: image_dir.to_path_buf(),
            runtime_defaults,
            rootfs_path,
            disk_manifest,
            size_bytes: total_size,
            disk_bootstrap_path,
        })
    }

    /// Push a freshly-baked image (output of [`Self::build`]) to a
    /// Docker registry as an Engram OCI artifact. Two layers:
    /// `manifest.toml` and `rootfs.ext4`. Returns the registry URI
    /// (suitable for storing in `image_versions.blob_url`) and the
    /// manifest's content digest.
    ///
    /// `registry_uri` may be either `host/repo:tag` (use this exact
    /// reference) or `host/repo` (auto-append `:<req.tag>`). The
    /// shorter form keeps the `engram image build --push <host/repo>`
    /// flow tidy.
    pub async fn push_to_registry(
        &self,
        req: &BuildRequest,
        outcome: &BuildOutcome,
        registry_uri: &str,
    ) -> Result<RegistryPush, BuildError> {
        let oci = self.oci.as_ref().ok_or_else(|| {
            BuildError::Config(
                "push_to_registry: no OCI client attached — call Builder::with_oci(...)".into(),
            )
        })?;
        // Only Ext4 outputs are pushable today — the Directory format
        // is fundamentally a per-host materialization (file ownership,
        // overlayfs whiteouts) that doesn't survive a tar+pull. The
        // rootfs.ext4 file *is* the artifact bytes.
        if !matches!(req.format, Format::Ext4) {
            return Err(BuildError::Config(
                "registry push currently requires --format ext4 (Directory bakes are local-only)"
                    .into(),
            ));
        }

        let full_uri = if registry_uri.contains(':') && !registry_uri.ends_with('/') {
            // Already has a tag (covers `host:port/repo:tag` and `host/repo:tag`).
            // Heuristic: a `:` after the last `/` is a tag separator. Otherwise
            // it's just a port on the host portion.
            let last_slash = registry_uri.rfind('/').unwrap_or(0);
            if registry_uri[last_slash..].contains(':') {
                registry_uri.to_string()
            } else {
                format!("{registry_uri}:{}", req.tag)
            }
        } else {
            format!("{registry_uri}:{}", req.tag)
        };

        // The artifact's config blob: introspection metadata (registries
        // display it) plus — ADR 0080 — the Dockerfile-derived
        // `runtime_defaults` the enable pipeline persists. The bake
        // carries NO other runtime config (engram.toml's manifest half is
        // retired; config arrives via `image enable --config`).
        let config = serde_json::json!({
            "kind": "engram-image-v1",
            "format": match req.format { Format::Ext4 => "ext4", Format::Directory => "directory" },
            "repo":   req.repo,
            "tag":    req.tag,
            "runtime_defaults": outcome.runtime_defaults,
        });
        let config_bytes = serde_json::to_vec(&config)
            .map_err(|e| BuildError::Config(format!("config json: {e}")))?;

        // ADR 0007 bundle layer — present iff the bake produced a
        // chunked disk manifest (Ext4 outputs do; Directory bakes
        // don't and can't be pushed anyway). Pullers learn the
        // chunk-manifest ref from this sidecar.
        let bundle_path = outcome.image_dir.join("bundle.json");
        let bundle_bytes = if outcome.disk_manifest.is_some() && bundle_path.exists() {
            Some(
                tokio::fs::read(&bundle_path)
                    .await
                    .map_err(BuildError::Io)?,
            )
        } else {
            None
        };

        // ADR 0036: when the bake produced a per-chunk bootstrap,
        // push a chunked OCI artifact — one blob per chunk, delta
        // style. HEAD each chunk digest and upload only the blobs
        // the registry is missing (a deterministic re-bake re-pushes
        // only its delta), then push the manifest referencing every
        // chunk layer. Failures cost one 16 MiB blob retry, never a
        // monolithic multi-GB upload session (the pre-ADR-0036
        // failure mode), and the artifact stays clear of registries'
        // per-layer size ceilings.
        if let (Some(bs_path), Some(bundle)) =
            (outcome.disk_bootstrap_path.as_ref(), bundle_bytes.as_ref())
        {
            let disk_bootstrap_json = tokio::fs::read(bs_path).await.map_err(BuildError::Io)?;
            let bootstrap: engram_chunk_store::Bootstrap =
                serde_json::from_slice(&disk_bootstrap_json)
                    .map_err(|e| BuildError::Config(format!("re-read disk bootstrap: {e}")))?;

            let (pushed, skipped) = self.push_chunk_blobs(oci, &full_uri, &bootstrap).await?;
            tracing::info!(
                uri = %full_uri,
                pushed,
                skipped,
                total = bootstrap.entries.len(),
                "chunk blobs delta-pushed to registry"
            );

            let chunk_refs: Vec<engram_oci::ChunkLayerRef> = bootstrap
                .entries
                .iter()
                .map(|e| {
                    Ok(engram_oci::ChunkLayerRef {
                        digest: e.blob_digest.clone().ok_or_else(|| {
                            BuildError::Config(format!(
                                "bootstrap entry {} missing per-chunk blob digest",
                                e.sha256
                            ))
                        })?,
                        size: e.length as u64,
                    })
                })
                .collect::<Result<_, BuildError>>()?;

            let layers = engram_oci::ChunkedImageLayers {
                config_json: config_bytes,
                bundle_json: bundle.clone(),
                disk_bootstrap_json,
            };
            let digest = oci
                .push_chunked_image_manifest(&full_uri, layers, &chunk_refs)
                .await
                .map_err(|e| BuildError::Docker(format!("oci chunked push: {e}")))?;

            return Ok(RegistryPush {
                uri: full_uri,
                manifest_digest: digest,
            });
        }

        // ADR 0007 Phase 6: skip the rootfs.ext4 layer when the
        // bundle is present. Disk bytes already live in the chunk
        // store (`Builder::build` chunked them at bake time); the
        // OCI layer is pure duplication. For a 4 GiB rootfs that's
        // 4 GiB of wasted registry bandwidth + storage per push.
        // No-bundle bakes (legacy, dev-only) still ship the full
        // ext4 layer as a fallback.
        let rootfs_arg: Option<&Path> = if bundle_bytes.is_some() {
            None
        } else {
            Some(outcome.rootfs_path.as_path())
        };

        let digest = oci
            .push_image(
                &full_uri,
                rootfs_arg,
                &config_bytes,
                bundle_bytes.as_deref(),
            )
            .await
            .map_err(|e| BuildError::Docker(format!("oci push: {e}")))?;

        Ok(RegistryPush {
            uri: full_uri,
            manifest_digest: digest,
        })
    }

    /// ADR 0036: delta-push every chunk in `bootstrap` as its own
    /// content-addressed OCI blob. For each entry: HEAD the digest
    /// (skip when the registry already has it — the cross-bake dedup
    /// that makes deterministic re-bakes upload only their delta),
    /// else read the chunk from the bake's chunk store and push it.
    ///
    /// Bounded concurrency keeps peak memory at
    /// `concurrency × chunk_size` (default 32 × 16 MiB ≈ 512 MiB)
    /// regardless of image size; per-blob retry with backoff means a
    /// transient blip costs one 16 MiB re-upload, not the whole push.
    /// The in-flight count is [`push_concurrency`] (env-tunable) —
    /// GHCR throttles per-connection, so a fat warm-image push stays
    /// network-bound rather than serialized behind a handful of streams.
    ///
    /// Returns `(pushed, skipped)` counts.
    async fn push_chunk_blobs(
        &self,
        oci: &engram_oci::OciClient,
        uri: &str,
        bootstrap: &engram_chunk_store::Bootstrap,
    ) -> Result<(usize, usize), BuildError> {
        use futures::stream::{FuturesUnordered, StreamExt};

        // One token mint for the whole run; concurrent first-pushes
        // would otherwise each eat a 401 + re-auth.
        oci.auth_for_push(uri)
            .await
            .map_err(|e| BuildError::Docker(format!("registry auth: {e}")))?;

        let concurrency = push_concurrency();
        const MAX_ATTEMPTS: u32 = 5;

        async fn push_one(
            oci: &engram_oci::OciClient,
            chunk_store: &engram_chunk_store::ChunkStore,
            uri: &str,
            entry: engram_chunk_store::BootstrapEntry,
            total: usize,
            done_so_far: usize,
        ) -> Result<bool, BuildError> {
            let digest = entry.blob_digest.as_deref().ok_or_else(|| {
                BuildError::Config(format!(
                    "bootstrap entry {} missing per-chunk blob digest",
                    entry.sha256
                ))
            })?;
            if oci
                .blob_exists(uri, digest)
                .await
                .map_err(|e| BuildError::Docker(format!("HEAD {digest}: {e}")))?
            {
                return Ok(false);
            }
            let bytes = chunk_store
                .get_chunk(entry.sha256)
                .await
                .map_err(|e| BuildError::Config(format!("read chunk {}: {e}", entry.sha256)))?;
            let mut attempt = 1u32;
            loop {
                match oci.push_chunk_blob(uri, digest, &bytes).await {
                    Ok(()) => break,
                    Err(e) if attempt < MAX_ATTEMPTS => {
                        // 429s want a politer pause than transient
                        // connection blips.
                        let rate_limited = e.to_string().contains("429");
                        let backoff_ms = if rate_limited { 5_000 } else { 500 } * attempt as u64;
                        tracing::warn!(
                            chunk = %entry.sha256,
                            attempt,
                            rate_limited,
                            error = %e,
                            "chunk blob push failed; retrying with backoff"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                        attempt += 1;
                    }
                    Err(e) => {
                        return Err(BuildError::Docker(format!(
                            "push chunk {} after {attempt} attempts: {e}",
                            entry.sha256
                        )));
                    }
                }
            }
            // Progress is operator-facing: bakes run in CI where this
            // log line is the only signal the push is alive.
            if done_so_far.is_multiple_of(25) {
                tracing::info!(done = done_so_far, total, "chunk push progress");
            }
            Ok(true)
        }

        let mut iter = bootstrap.entries.iter().enumerate();
        let mut tasks = FuturesUnordered::new();
        let mut pushed = 0usize;
        let mut skipped = 0usize;
        for _ in 0..concurrency {
            if let Some((i, entry)) = iter.next() {
                tasks.push(push_one(
                    oci,
                    &self.chunk_store,
                    uri,
                    entry.clone(),
                    bootstrap.entries.len(),
                    i,
                ));
            }
        }
        while let Some(res) = tasks.next().await {
            match res? {
                true => pushed += 1,
                false => skipped += 1,
            }
            if let Some((i, entry)) = iter.next() {
                tasks.push(push_one(
                    oci,
                    &self.chunk_store,
                    uri,
                    entry.clone(),
                    bootstrap.entries.len(),
                    i,
                ));
            }
        }
        Ok((pushed, skipped))
    }
}

/// In-flight concurrent blob uploads for a chunked-image push.
///
/// Default 32; override via `ENGRAM_PUSH_CONCURRENCY`. GHCR throttles
/// per-connection (warm-image bakes observe ~1.8 MB/s/stream), so the
/// aggregate push rate scales with concurrency until the registry's
/// own ceiling — bump the env var to probe it. Peak memory is
/// `N × 16 MiB` chunk, so a big N is RAM, not CPU, bound. A junk or
/// `0` value falls back to the default rather than wedging the push.
fn push_concurrency() -> usize {
    parse_push_concurrency(std::env::var("ENGRAM_PUSH_CONCURRENCY").ok())
}

/// Pure core of [`push_concurrency`], split out so the parse/clamp is
/// unit-testable without poking the process environment.
fn parse_push_concurrency(raw: Option<String>) -> usize {
    const DEFAULT: usize = 32;
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT)
}

/// Result of a successful push to an OCI registry.
#[derive(Clone, Debug)]
pub struct RegistryPush {
    pub uri: String,
    pub manifest_digest: engram_oci::Digest256,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ENGRAM_PUSH_CONCURRENCY` parsing: a valid override wins, but
    /// unset / junk / zero all fall back to the default (32) rather
    /// than wedging the push at 0 in-flight uploads.
    #[test]
    fn push_concurrency_parse_and_clamp() {
        assert_eq!(parse_push_concurrency(None), 32, "unset → default");
        assert_eq!(
            parse_push_concurrency(Some("64".into())),
            64,
            "valid override"
        );
        assert_eq!(parse_push_concurrency(Some(" 16 ".into())), 16, "trimmed");
        assert_eq!(
            parse_push_concurrency(Some("0".into())),
            32,
            "0 → default, never 0 streams"
        );
        assert_eq!(
            parse_push_concurrency(Some("nope".into())),
            32,
            "junk → default"
        );
        assert_eq!(
            parse_push_concurrency(Some("".into())),
            32,
            "empty → default"
        );
    }
}
