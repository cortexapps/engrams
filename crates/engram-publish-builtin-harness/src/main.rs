//! `engram-publish-builtin-harness` — CI-only publisher for the
//! built-in harness OCI artifacts that `engram image build` pulls
//! and injects when an `engram.toml` declares
//! `[harness] builtin = "<name>" version = "<v>"`.
//!
//! Replaces the retired `engram-cli harness push` subcommand
//! (ADR 0021 P1.5a deleted the rest of the harness CLI surface).
//! Called from the `bake-harness-claude-artifact` job in
//! `.github/workflows/ci.yml` — on push-to-main to publish the
//! production GHCR tag, and from the `test-e2e-stack` job to
//! republish a downloaded workflow artifact into the integration
//! lane's local registry (localhost:5001).
//!
//! Usage:
//!
//! ```sh
//! engram-publish-builtin-harness \
//!   --from <stage-dir> \
//!   --to ghcr.io/cortexapps/engrams/harness-claude:v0.1.0-linux-x86_64
//! ```
//!
//! `<stage-dir>` is the contents of `/opt/engram/harness/` the baker
//! will extract on the image side: the entry-point binary at
//! `harness`, optional sidecars, and an `artifact.toml` descriptor.
//!
//! Auth is the same shape as the rest of engram's OCI plumbing —
//! `DockerConfigResolver` reads `~/.docker/config.json`, so a
//! prior `docker login` (or CI's `docker/login-action`) is enough.
//! With no config the resolver returns no creds and the push falls
//! through to anonymous (works for public registries and
//! `localhost:5001`).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use engram_oci::{DockerConfigResolver, OciClient};

#[derive(Parser, Debug)]
#[command(
    name = "engram-publish-builtin-harness",
    version,
    about = "Push a built-in harness OCI artifact (CI-only)"
)]
struct Cli {
    /// Local directory whose contents become the tar.gz layer of the
    /// OCI artifact. Should be shaped as `/opt/engram/harness/`: at
    /// least an executable `harness` entry-point, plus any sidecars
    /// and an `artifact.toml` descriptor.
    #[arg(long)]
    from: PathBuf,

    /// Destination OCI URI, e.g.
    /// `ghcr.io/cortexapps/engrams/harness-claude:v0.1.0-linux-x86_64`.
    #[arg(long)]
    to: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    if !cli.from.exists() {
        eprintln!(
            "engram-publish-builtin-harness: --from {} does not exist",
            cli.from.display()
        );
        return ExitCode::from(2);
    }

    let oci = OciClient::new(Arc::new(DockerConfigResolver::new()));
    tracing::info!(uri = %cli.to, dir = %cli.from.display(), "pushing built-in harness");
    let digest = match oci.push_harness(&cli.to, &cli.from).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("engram-publish-builtin-harness: push failed: {e}");
            return ExitCode::from(1);
        }
    };
    println!("✓ pushed {}", cli.to);
    println!("  digest: {}", digest.as_str());
    ExitCode::SUCCESS
}
