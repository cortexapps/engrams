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
use engram_postgres::PostgresStore;

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

    /// Optional. If set, upserts the produced `image_versions` row.
    /// Without it the bake is filesystem-only (handy for `engram-cli
    /// image build` against a coordinator-less workspace).
    #[arg(long, env = "DATABASE_URL")]
    database_url: Option<String>,

    /// Override the docker binary (e.g. `podman`).
    #[arg(long, env = "ENGRAM_DOCKER_BIN")]
    docker_bin: Option<String>,

    /// Output format. `directory` (default) produces a directory tree
    /// for the dev backend; `ext4` produces a `rootfs.ext4` block-
    /// device image for Firecracker.
    #[arg(long, value_parser = parse_format, default_value = "directory")]
    format: Format,
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
        // Agent injection isn't exposed on the CLI yet — Phase 2's
        // real-microVM exec test drives it via the library. Adding a
        // `--inject-agent` flag is a CLI ergonomics task for later.
        agent_injection: None,
    };

    let docker = match opts.docker_bin {
        Some(bin) => DockerCli::with_binary(bin),
        None => DockerCli::new(),
    };
    let builder = Builder::new(docker);

    let outcome = builder.build(&req).await?;
    println!("{}", outcome.image_dir.display());

    if let Some(url) = opts.database_url.as_deref() {
        let pg = PostgresStore::connect(url).await?;
        builder.record_in_metadata(&pg, &req, &outcome).await?;
        tracing::info!(repo = %req.repo, tag = %tag, "image_versions row marked Ready");
    } else {
        tracing::info!(
            repo = %req.repo,
            tag = %tag,
            "DATABASE_URL not set — skipping metadata write (filesystem-only bake)",
        );
    }

    Ok(())
}
