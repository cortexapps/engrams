//! `engram-disk-source serve`: serve one disk manifest as a read-only
//! ublk device until SIGTERM or SIGINT, then delete the device. Its state
//! is a function of its arguments and the blob-store environment
//! (`ENGRAM_BLOB_BACKEND` and the backend's settings), so a new process
//! with the same arguments and `--recover` takes over a device whose
//! process died.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use engram_chunk_store::ManifestRef;

#[derive(Parser)]
#[command(
    name = "engram-disk-source",
    about = "Serve a disk manifest as a read-only block device"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create (or recover) the device, print "<device path> <device id>"
    /// on one line when it serves I/O, and serve until SIGTERM or SIGINT.
    Serve(ServeArgs),
}

#[derive(Args)]
struct ServeArgs {
    /// The disk manifest to serve, as <uuid>@v<version>.
    #[arg(long)]
    manifest: ManifestRef,
    /// A local volume to read chunks from when it holds a chunk with the
    /// same hash.
    #[arg(long, requires = "base_manifest")]
    base_volume: Option<PathBuf>,
    /// The manifest the base volume holds, as <uuid>@v<version>.
    #[arg(long, requires = "base_volume")]
    base_manifest: Option<ManifestRef>,
    /// The host chunk cache to read resident chunks from. It is never
    /// written.
    #[arg(long)]
    cache_root: Option<PathBuf>,
    /// Attach to the existing device with this id instead of creating one.
    #[arg(long)]
    recover: Option<u32>,
    /// Hardware queues. Ignored with --recover: the device keeps its own.
    #[arg(long, default_value_t = 2)]
    queues: u16,
    /// Requests in flight per queue. Ignored with --recover.
    #[arg(long, default_value_t = 64)]
    depth: u16,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let Command::Serve(args) = Cli::parse().command;
    match serve(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("engram-disk-source: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn serve(_args: ServeArgs) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "ublk block devices need Linux",
    ))
}

#[cfg(target_os = "linux")]
fn serve(args: ServeArgs) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::Arc;

    use engram_disk_source::ublk;
    use tokio::signal::unix::{signal, SignalKind};

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // Before the device exists, so a stop that arrives during start-up
    // still deletes it.
    let (mut term, mut int) = {
        let _guard = rt.enter();
        (
            signal(SignalKind::terminate())?,
            signal(SignalKind::interrupt())?,
        )
    };
    let stop = async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    };
    let reader = rt.block_on(open_reader(&args))?;
    let config = ublk::Config {
        queues: args.queues,
        depth: args.depth,
        recover: args.recover,
    };
    let on_ready = |device: &ublk::Device| {
        tracing::info!(dev_id = device.dev_id, path = %device.path, "serving");
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{} {}", device.path, device.dev_id);
        let _ = out.flush();
    };
    ublk::serve(
        Arc::new(reader),
        rt.handle().clone(),
        &config,
        on_ready,
        stop,
    )
}

#[cfg(target_os = "linux")]
async fn open_reader(args: &ServeArgs) -> std::io::Result<engram_disk_source::DiskReader> {
    use engram_chunk_store::reader::ChunkCacheReader;
    use engram_chunk_store::ChunkStore;
    use engram_disk_source::{DiskReader, LocalBase};

    let blob = engram_blob_client::from_env()
        .await
        .map_err(std::io::Error::other)?;
    let store = ChunkStore::new(blob);
    let manifest = store
        .get_manifest(args.manifest)
        .await
        .map_err(std::io::Error::other)?;
    let mut reader = DiskReader::new(&manifest, store.clone())?;
    if let (Some(path), Some(base_ref)) = (&args.base_volume, args.base_manifest) {
        let base_manifest = store
            .get_manifest(base_ref)
            .await
            .map_err(std::io::Error::other)?;
        reader = reader.with_base(LocalBase::open(path, &base_manifest)?);
    }
    if let Some(root) = &args.cache_root {
        reader = reader.with_cache(ChunkCacheReader::new(root));
    }
    Ok(reader)
}
