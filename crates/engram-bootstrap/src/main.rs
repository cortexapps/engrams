//! In-guest bootstrap shim.
//!
//! Lives at `/sbin/engram-bootstrap` inside the rootfs. Spawned in
//! the background by `/sbin/engram-init` alongside `engram-agentd`,
//! waits for the host to push a [`BootstrapLaunch`] frame over vsock
//! port [`BOOTSTRAP_VSOCK_PORT`], then `exec`s the described argv
//! (with merged env) so the per-session agent process — typically a
//! harness adapter — takes over.
//!
//! This indirection exists because the warm pool's `SandboxSpec` is
//! agent-blind: per-session argv (carrying `session_id`, attach
//! token, etc.) can't ride on the spec template. The host populates
//! the per-session AgentSpec at `start_agent` time, after the
//! coordinator has bound the session→sandbox routing in the
//! HarnessHub. Bootstrap is the in-VM half of that handoff.

// engram-bootstrap is Linux-only — vsock is a Linux kernel feature.
// Cross-platform stub keeps the workspace cargo check / cargo nextest
// path on macOS / non-Linux clean while the real binary builds via
// `cargo build -p engram-bootstrap --target x86_64-unknown-linux-musl`.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("engram-bootstrap is Linux-only");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "linux")]
use std::process::{Command, ExitCode};

#[cfg(target_os = "linux")]
use engram_harness_proto::{read_msg, BootstrapLaunch, BOOTSTRAP_VSOCK_PORT};
#[cfg(target_os = "linux")]
use tokio_vsock::{VsockAddr, VsockListener, VMADDR_CID_ANY};

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Stay inside a single-threaded runtime so we can `exec` cleanly
    // — multi-threaded tokio holds OS threads that block exec.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let addr = VsockAddr::new(VMADDR_CID_ANY, BOOTSTRAP_VSOCK_PORT);
    let mut listener = match VsockListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, port = BOOTSTRAP_VSOCK_PORT, "bind vsock listener failed");
            return ExitCode::from(1);
        }
    };
    tracing::info!(
        port = BOOTSTRAP_VSOCK_PORT,
        "engram-bootstrap waiting for host launch",
    );

    // The host opens the FC vsock UDS, writes `CONNECT 1025\n`, and
    // drops the BootstrapLaunch frame on the resulting stream. We
    // accept exactly one connection — once we've received the
    // launch we exec away.
    let (mut stream, peer) = match listener.accept().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "accept failed");
            return ExitCode::from(1);
        }
    };
    tracing::debug!(?peer, "accepted host bootstrap connection");

    let launch: BootstrapLaunch = match read_msg(&mut stream).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, "read BootstrapLaunch failed");
            return ExitCode::from(1);
        }
    };
    drop(stream);
    drop(listener);

    let argv0 = match launch.argv.first() {
        Some(a) => a.clone(),
        None => {
            tracing::error!("BootstrapLaunch.argv was empty");
            return ExitCode::from(1);
        }
    };
    tracing::info!(
        argv0 = %argv0,
        argc = launch.argv.len(),
        envc = launch.env.len(),
        "exec'ing into agent",
    );

    let mut cmd = Command::new(&argv0);
    cmd.args(&launch.argv[1..]);
    for (k, v) in &launch.env {
        cmd.env(k, v);
    }
    // exec replaces our image. Anything past this line means exec
    // failed — surface a clear error and exit 1.
    let err = cmd.exec();
    tracing::error!(error = %err, argv0 = %argv0, "exec failed");
    ExitCode::from(1)
}
