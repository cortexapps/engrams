//! In-guest bootstrap supervisor.
//!
//! Lives at `/sbin/engram-bootstrap` inside the rootfs. Spawned in
//! the background by `/sbin/engram-init` alongside `engram-agentd`,
//! listens on the host↔guest control transport (vsock or
//! virtio-console — selected at boot via `ENGRAM_TRANSPORT`) for
//! [`BootstrapLaunch`] frames from the host, and runs the described
//! agent process as a child. Each new launch frame kills the previous
//! child and starts a fresh one — so the host has a clean way to
//! re-establish the per-session agent (e.g. after FC snapshot/restore
//! invalidates the previous adapter's connection).
//!
//! This indirection exists because `SandboxSpec` is agent-blind:
//! per-session argv (carrying `session_id`, attach token, etc.) can't
//! ride on the spec template. The host populates the per-session
//! AgentSpec at `start_agent` time, after the coordinator has bound
//! the session→sandbox routing in the HarnessHub.
//!
//! Why supervise instead of `exec`. Earlier revisions exec'd into
//! the harness so bootstrap exited and didn't hang around. That broke
//! post-resume reconnect: an FC snapshot/restore round-trip leaves
//! the in-VM adapter holding a half-open connection it can't detect,
//! with no in-VM process for the host to talk to. As a supervisor,
//! bootstrap is always running with a live listener; the host can
//! re-deliver a `BootstrapLaunch` frame after restore and bootstrap
//! kill+respawns the adapter for a clean reattach.

// engram-bootstrap is Linux-only — both vsock and virtio-console are
// Linux kernel features. Cross-platform stub keeps the workspace
// cargo check / cargo nextest path on macOS / non-Linux clean while
// the real binary builds via
// `cargo build -p engram-bootstrap --target *-unknown-linux-musl`.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("engram-bootstrap is Linux-only");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
use std::process::ExitCode;

#[cfg(target_os = "linux")]
use engram_harness_proto::{read_msg, BootstrapLaunch, BOOTSTRAP_VSOCK_PORT};
#[cfg(target_os = "linux")]
use tokio::process::{Child, Command};

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let transport = match engram_transport::from_env() {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "build transport from ENGRAM_TRANSPORT failed");
            return ExitCode::from(1);
        }
    };
    let mut listener = match transport.listen(BOOTSTRAP_VSOCK_PORT).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, port = BOOTSTRAP_VSOCK_PORT, "bind transport listener failed");
            return ExitCode::from(1);
        }
    };
    tracing::info!(
        port = BOOTSTRAP_VSOCK_PORT,
        "engram-bootstrap supervisor listening",
    );

    // Most recent agent child. Kept across iterations so we can
    // SIGKILL+wait it before honoring a new launch frame — without
    // this, a post-resume relaunch would leave the pre-snapshot
    // adapter zombied alongside the fresh one, fighting over
    // /workspace/.engram/claude-session-id.
    let mut current_child: Option<Child> = None;

    // Outer loop: re-accept on each host-side reconnect (vsock
    // gives a fresh stream per accept; virtio-console reopens
    // /dev/hvcN — same shape from this side).
    'accept: loop {
        let mut stream = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "accept failed; supervisor exiting");
                if let Some(mut child) = current_child {
                    let _ = child.kill().await;
                }
                return ExitCode::from(1);
            }
        };
        // INFO so the FC console log (the only post-mortem we have
        // for in-guest hangs) shows whether accept fired. Without
        // this, a 15s start_agent timeout looks identical whether
        // bootstrap accepted-but-didn't-write or never-accepted.
        tracing::info!("accepted host bootstrap connection");

        // Tiny readiness handshake: write a single byte to the host
        // the moment we open this stream. Without it the host's
        // start_agent would race the cold-boot bootstrap startup —
        // on virtio-console (VZ) the host's BootstrapLaunch frame
        // can arrive before the guest's port is opened, and the
        // bytes get dropped on the floor (VZ doesn't replay buffered
        // writes when the guest's port comes up). The host's
        // start_agent reads this byte before writing the launch
        // frame, guaranteeing we're ready to consume it.
        use tokio::io::AsyncWriteExt;
        if let Err(e) = stream
            .write_all(&[engram_harness_proto::BOOTSTRAP_READY_BYTE])
            .await
        {
            tracing::warn!(error = %e, "writing bootstrap ready byte failed");
            continue 'accept;
        }

        // Inner loop: read frames off this stream until EOF. For
        // vsock we typically see one frame per stream then EOF
        // (host closes after writing); for virtio-console the host
        // can write multiple frames over a single open before
        // closing.
        loop {
            let launch: BootstrapLaunch = match read_msg(&mut stream).await {
                Ok(l) => l,
                Err(e) => {
                    let kind = e.kind();
                    if matches!(
                        kind,
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionReset
                    ) {
                        tracing::debug!(error = %e, "stream closed by host; awaiting next connection");
                    } else {
                        tracing::warn!(error = %e, "read BootstrapLaunch failed; awaiting next connection");
                    }
                    continue 'accept;
                }
            };

            // Kill any prior child. We send SIGKILL rather than
            // SIGTERM to keep the supervisor simple and predictable:
            // the previous adapter's state is presumed stale (post-
            // resume case) so we don't owe it a graceful shutdown
            // window.
            if let Some(mut prev) = current_child.take() {
                tracing::info!(
                    pid = ?prev.id(),
                    "respawn requested; killing previous agent child",
                );
                let _ = prev.kill().await;
                let _ = prev.wait().await;
            }

            let argv0 = match launch.argv.first() {
                Some(a) => a.clone(),
                None => {
                    tracing::warn!("BootstrapLaunch.argv was empty; ignoring");
                    continue;
                }
            };
            tracing::info!(
                argv0 = %argv0,
                argc = launch.argv.len(),
                envc = launch.env.len(),
                harness_dev = ?launch.harness_dev,
                harness_mount = ?launch.harness_mount,
                "spawning agent",
            );

            // ADR 0014 M1.12 (option D): bootstrap mounts the harness
            // device the host nominated. We do this here rather than
            // in engram-init so warm-pool templates can be harness-
            // agnostic — the template snapshot captures bootstrap on
            // accept() before any harness mount has happened. The
            // host PATCHes the harness drive per session, then sends
            // a BootstrapLaunch frame pointing at the new device.
            //
            // engram-init left the mount point in place (idempotent
            // mkdir), so we just need to call mount(2). Best-effort:
            // a malformed harness ext4 surfaces a clear error in the
            // serial console + we skip the exec so the session fails
            // fast with no half-running agent.
            let mut harness_mounted_at: Option<String> = None;
            if let (Some(dev), Some(mount_point)) =
                (launch.harness_dev.as_ref(), launch.harness_mount.as_ref())
            {
                if let Err(e) = mount_harness(dev, mount_point) {
                    tracing::error!(
                        error = %e,
                        dev = %dev,
                        mount = %mount_point,
                        "harness mount failed; skipping spawn",
                    );
                    continue;
                }
                tracing::info!(
                    dev = %dev,
                    mount = %mount_point,
                    "harness mounted",
                );
                harness_mounted_at = Some(mount_point.clone());
            }

            let mut cmd = Command::new(&argv0);
            cmd.args(&launch.argv[1..]);
            for (k, v) in &launch.env {
                cmd.env(k, v);
            }

            // ADR 0014 M1.12: egress-proxy CA setup. Pre-M1.12 this
            // ran in engram-init; moved here because the harness
            // substrate (where the host stamps the CA at
            // `.engram-host/ca.pem`) is mounted by bootstrap, not by
            // init. Idempotent: each session gets a fresh CoW
            // rootfs, so the append-to-ca-certificates.crt is
            // per-session and can't accumulate.
            if let Some(mount) = harness_mounted_at.as_ref() {
                if let Err(e) = inject_egress_proxy_ca(mount, &mut cmd) {
                    tracing::warn!(
                        error = %e,
                        mount = %mount,
                        "egress-proxy CA setup failed; harness will not trust proxy-minted leaves",
                    );
                }
            }
            match cmd.spawn() {
                Ok(child) => {
                    tracing::info!(pid = ?child.id(), "agent child running");
                    current_child = Some(child);
                }
                Err(e) => {
                    tracing::error!(error = %e, argv0 = %argv0, "spawn failed");
                    // Don't exit — the host might recover and push
                    // a different launch frame. Just leave
                    // current_child unset for the next iteration.
                }
            }
        }
    }
}

/// ADR 0014 M1.12: mount(2) wrapper for the harness device. ext4 +
/// read-only matches what engram-init used to do; the device is the
/// session's harness substrate attached at FC bake time and (for
/// warm leases) hot-swapped via FC `PATCH /drives`.
///
/// Idempotent: if the mount point is already a mount of the same
/// device, returns Ok. The kernel handles "already mounted" via
/// EBUSY; we treat it as success because re-entering bootstrap on
/// the same warm slot (rare; only happens if the agent died and the
/// host sent a fresh launch frame) shouldn't fail the mount.
#[cfg(target_os = "linux")]
fn mount_harness(dev: &str, mount_point: &str) -> std::io::Result<()> {
    use nix::mount::{mount, MsFlags};

    // Mount point may not exist on the bake-time stub path. Create
    // it idempotently.
    if let Err(e) = std::fs::create_dir_all(mount_point) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e);
        }
    }

    // Same flags engram-init used pre-M1.12: ext4 + read-only. No
    // mount data (ext4 has sensible defaults; option-D's stub
    // harness ext4 is single-file and doesn't need a custom data
    // arg).
    match mount(
        Some(dev),
        mount_point,
        Some("ext4"),
        MsFlags::MS_RDONLY,
        None::<&str>,
    ) {
        Ok(()) => Ok(()),
        // EBUSY = already mounted there. Idempotent on re-entry,
        // which can happen if the agent died and the host sent a
        // fresh BootstrapLaunch on the same warm slot.
        Err(nix::errno::Errno::EBUSY) => Ok(()),
        Err(e) => Err(std::io::Error::from_raw_os_error(e as i32)),
    }
}

/// ADR 0014 M1.12: egress-proxy CA setup, moved here from engram-
/// init because the harness substrate (where the host stamps the CA
/// at `.engram-host/ca.pem`) is now mounted by bootstrap.
///
/// Appends the CA to the system bundle (`/etc/ssl/certs/ca-
/// certificates.crt`) so glibc-based tools trust proxy leaves
/// without per-tool env vars, and injects the env-var family that
/// covers curl, requests, and Node's TLS stack. No-op when the CA
/// file isn't present (egress proxy not deployed for this session).
#[cfg(target_os = "linux")]
fn inject_egress_proxy_ca(harness_mount: &str, cmd: &mut Command) -> std::io::Result<()> {
    let ca_path = format!("{harness_mount}/.engram-host/ca.pem");
    if !std::path::Path::new(&ca_path).exists() {
        return Ok(());
    }
    let bundle = "/etc/ssl/certs/ca-certificates.crt";
    let ca_bytes = std::fs::read(&ca_path)?;
    // Append (or create) — matches the pre-M1.12 shell behavior.
    let _ = std::fs::create_dir_all("/etc/ssl/certs");
    let mut bundle_bytes = std::fs::read(bundle).unwrap_or_default();
    if !bundle_bytes.ends_with(b"\n") && !bundle_bytes.is_empty() {
        bundle_bytes.push(b'\n');
    }
    bundle_bytes.extend_from_slice(&ca_bytes);
    std::fs::write(bundle, &bundle_bytes)?;

    cmd.env("SSL_CERT_FILE", bundle);
    cmd.env("CURL_CA_BUNDLE", bundle);
    cmd.env("REQUESTS_CA_BUNDLE", bundle);
    cmd.env("NODE_EXTRA_CA_CERTS", &ca_path);
    tracing::info!(ca = %ca_path, bundle, "egress-proxy CA installed into trust store");
    Ok(())
}
