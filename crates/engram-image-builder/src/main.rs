//! `engram-image-builder` binary. Bakes one image per invocation;
//! cron-driven in production. Talks to Docker on the host (Docker
//! Desktop, OrbStack, Colima, Podman with docker-compat).
//!
//! Usage:
//!
//! ```text
//! engram-image-builder build \
//!   --repo cortex/api \
//!   --source ./path/to/repo \
//!   --images-dir ./var/snapshots/images \
//!   --database-url postgres://...
//! ```
//!
//! The repo at `--source` must contain `Dockerfile` (or whatever the
//! `[build].dockerfile` field of `engram.toml` says) and `engram.toml`.

use std::path::PathBuf;

use chrono::Utc;
use clap::{Parser, Subcommand};
use engram_image_builder::{BuildRequest, Builder, DockerCli, Format};
use engram_oci::{DockerConfigResolver, OciClient};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "engram-image-builder", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Bake a single image from a source repo.
    Build(BuildOpts),
}

#[derive(Parser, Debug)]
struct BuildOpts {
    /// Repo identifier (e.g. `cortex/api`). Determines the registry
    /// path the produced image lands at.
    #[arg(long)]
    repo: String,

    /// Path to the source repo (containing `Dockerfile` + `engram.toml`).
    #[arg(long, default_value = ".")]
    source: PathBuf,

    /// Override the produced tag. Default: `warm-<rfc3339>`.
    #[arg(long)]
    tag: Option<String>,

    /// Where the registry lives — must match the coordinator's
    /// `<storage_local_path>/images`. Default: `./var/snapshots/images`.
    #[arg(
        long,
        env = "ENGRAM_IMAGES_DIR",
        default_value = "./var/snapshots/images"
    )]
    images_dir: PathBuf,

    /// Override the docker binary (e.g. `podman`).
    #[arg(long, env = "ENGRAM_DOCKER_BIN")]
    docker_bin: Option<String>,

    /// Output format. `directory` (default) produces a directory tree
    /// for the dev backend; `ext4` produces a `rootfs.ext4` block-
    /// device image for Firecracker.
    #[arg(long, value_parser = parse_format, default_value = "directory")]
    format: Format,

    /// Push the baked image to a Docker registry as an Engram OCI
    /// artifact. Accepts either `host/repo` (auto-appends the produced
    /// tag) or `host/repo:tag`. Requires `--format ext4`.
    ///
    /// The bake step intentionally does NOT touch Postgres — once
    /// pushed, an image is reachable to engram by URI alone (the
    /// host-agent pulls via the auth resolver at session-create time).
    #[arg(long)]
    push: Option<String>,
}

fn parse_format(s: &str) -> Result<Format, String> {
    match s {
        "directory" | "dir" => Ok(Format::Directory),
        "ext4" => Ok(Format::Ext4),
        other => Err(format!(
            "unknown format `{other}`; expected `directory` or `ext4`"
        )),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,engram=debug")),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Build(opts) => run_build(opts).await,
    }
}

async fn run_build(opts: BuildOpts) -> Result<(), Box<dyn std::error::Error>> {
    let tag = opts
        .tag
        .clone()
        .unwrap_or_else(|| format!("warm-{}", Utc::now().format("%Y%m%dT%H%M%SZ")));
    let req = BuildRequest {
        source: opts.source.clone(),
        repo: opts.repo.clone(),
        tag: tag.clone(),
        images_dir: opts.images_dir.clone(),
        format: opts.format,
        // Init-shim injection isn't exposed on this standalone binary —
        // `engram-cli image build --inject-init` is the real entry point;
        // this bin serves the dev/directory-format path only.
        init_injection: None,
    };

    let docker = match opts.docker_bin {
        Some(bin) => DockerCli::with_binary(bin),
        None => DockerCli::new(),
    };

    // ADR 0007: chunk-store backend driven by `ENGRAM_BLOB_BACKEND`.
    //   - `local` (default): chunks land at `<images_dir>/store/`,
    //     keeping the dev workflow self-contained.
    //   - `gcs`: chunks land directly in the deployment bucket; the
    //     CI runner needs `ENGRAM_GCS_BUCKET` set and the right
    //     Workload Identity binding. Without this knob, production
    //     bakes wrote to the runner's local FS and lost the chunks
    //     on recycle — see docs/chunked-storage-rollout.md.
    let blob = engram_image_builder::blob::from_env(&opts.images_dir).await?;
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    // One OCI client, set up once and used for both built-in harness
    // pulls (during `build`) and the optional registry push (after
    // `build`). Auth is read from the standard docker config
    // (`$DOCKER_CONFIG/config.json` or `~/.docker/config.json`); run
    // `docker login <registry>` upstream or let CI's login-action
    // populate it. With no docker config the resolver returns no
    // creds and pull/push fall through to anonymous — matches the
    // previous behaviour for public/local registries.
    let oci = OciClient::new(Arc::new(DockerConfigResolver::new()));
    let builder = Builder::new(docker, chunk_store).with_oci(oci);

    let outcome = builder.build(&req).await?;
    println!("{}", outcome.image_dir.display());

    if let Some(target) = opts.push.as_deref() {
        let push = builder.push_to_registry(&req, &outcome, target).await?;
        tracing::info!(uri = %push.uri, digest = %push.manifest_digest.as_str(), "pushed to registry");
    }

    Ok(())
}
