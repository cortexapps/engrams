//! True end-to-end harness tests using the REAL `engram-harness-claude`
//! wrapper + the REAL Claude Code CLI binary, talking to the REAL
//! `api.anthropic.com` through the REAL `engram-egress-proxy`.
//!
//! Coverage matrix:
//!   - e2e_harness_cold: cold-created FC sandbox, harness starts,
//!     makes an outbound HTTPS request to api.anthropic.com through
//!     the proxy, and the proxy intercepts (cold-path iptables
//!     REDIRECT on tap-engr-+).
//!   - e2e_harness_warm: cold → snapshot → destroy → restore (which
//!     puts the VM in a per-VM netns), then the same harness call
//!     traverses the warm-path REDIRECT (vh-engr-+) + netns SNAT
//!     to reach the proxy. Catches the policy.guest_ip overwrite
//!     gap (`WarmPool::launch` Fix 3 territory) too — without the
//!     overwrite the proxy's `Registry::lookup(snat_ip)` would
//!     miss and the harness's outbound would be dropped.
//!
//! The bogus token causes Claude's API to 401, but THAT'S FINE —
//! the test asserts that an event makes it back through the
//! harness vsock channel. Whether the event content is a success
//! response or an error, it proves the full chain ran:
//!
//!   prompt env → engram-harness-claude → claude CLI → curl →
//!   in-VM eth0 → TAP (cold) or TAP-in-netns (warm) → REDIRECT
//!   → engram-egress-proxy → SystemResolver → api.anthropic.com →
//!   response → claude CLI parses → harness emits HarnessEvent
//!   → vsock → host's HarnessSink → our captured Vec.
//!
//! Run on the dev VM:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-host-agent --test e2e_harness -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{AgentSpec, CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_image_builder::{AgentInjection, BuildRequest, Builder, DockerCli, Format, Transport};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};
use parking_lot::Mutex;
use tokio::time::sleep;

/// Hosts the egress proxy is allowed to forward to. api.anthropic.com
/// is what the real Claude Code CLI dials; we allow it so the proxy
/// intercepts cleanly. statsig.anthropic.com is the telemetry host
/// the CLI also dials; allow it so a failed telemetry call doesn't
/// surface as a confusing test failure.
const ALLOW_HOSTS: &[&str] = &["api.anthropic.com", "statsig.anthropic.com"];

struct FcEnv {
    kernel: PathBuf,
    #[allow(dead_code)]
    rootfs: PathBuf,
}

fn fc_preflight() -> Option<FcEnv> {
    let kernel = std::env::var("FC_TEST_KERNEL").ok()?;
    let rootfs = std::env::var("FC_TEST_ROOTFS").ok()?;
    let kp = PathBuf::from(&kernel);
    if !kp.exists() {
        eprintln!("SKIP: FC_TEST_KERNEL={kernel} doesn't exist");
        return None;
    }
    let rp = PathBuf::from(&rootfs);
    if !rp.exists() {
        eprintln!("SKIP: FC_TEST_ROOTFS={rootfs} doesn't exist");
        return None;
    }
    Some(FcEnv {
        kernel: kp,
        rootfs: rp,
    })
}

fn require_root() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").expect("read status");
    let euid: u32 = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2).and_then(|s| s.parse().ok()))
        .expect("parse euid");
    if euid != 0 {
        eprintln!(
            "SKIP: e2e_harness tests require root (CAP_NET_ADMIN for TAP + iptables). \
             Re-run via `sudo -E ...`."
        );
        return false;
    }
    true
}

fn cleanup_host_state() {
    let saved = String::from_utf8(
        std::process::Command::new("iptables-save")
            .output()
            .expect("iptables-save")
            .stdout,
    )
    .unwrap_or_default();
    let mut current_table = "filter".to_string();
    for line in saved.lines() {
        if let Some(t) = line.strip_prefix('*') {
            current_table = t.trim().to_string();
            continue;
        }
        if !line.contains("engram-") {
            continue;
        }
        let Some(rest) = line.strip_prefix("-A ") else {
            continue;
        };
        let mut argv = vec!["-t".to_string(), current_table.clone(), "-D".to_string()];
        argv.extend(rest.split_whitespace().map(str::to_string));
        let _ = std::process::Command::new("iptables").args(&argv).output();
    }
    for pat in ["tap-engr-", "vh-engr-"] {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "ip -o link show | awk -F': ' '/{pat}/ {{print $2}}' | awk '{{print $1}}'"
            ))
            .output();
        let Ok(o) = out else { continue };
        for name in String::from_utf8_lossy(&o.stdout).lines() {
            let name = name.trim();
            if !name.is_empty() {
                let _ = std::process::Command::new("ip")
                    .args(["link", "delete", name])
                    .output();
            }
        }
    }
    for line in std::process::Command::new("ip")
        .args(["netns", "list"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
        .lines()
    {
        if let Some(name) = line.split_whitespace().next() {
            if name.starts_with("engr-vm-") {
                let _ = std::process::Command::new("ip")
                    .args(["netns", "delete", name])
                    .output();
            }
        }
    }
}

/// Build the musl engram-harness-claude binary + download the
/// Claude Code CLI, both host-side. Returns the paths.
async fn ensure_harness_artifacts() -> (PathBuf, PathBuf) {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let harness_bin = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("engram-harness-claude");
    assert!(
        harness_bin.exists(),
        "musl engram-harness-claude not at {} — \
         run `cargo build -p engram-harness-claude --target x86_64-unknown-linux-musl --release` \
         before this test",
        harness_bin.display(),
    );

    // Download the Claude Code CLI to a cache dir keyed by the
    // upstream's `latest` pointer so repeated test runs don't
    // re-download.
    let cache_dir = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".cache/engram-e2e-harness"))
        .unwrap_or_else(|| PathBuf::from("/tmp/engram-e2e-harness"));
    std::fs::create_dir_all(&cache_dir).unwrap();
    let claude_bin = cache_dir.join("claude");
    if !claude_bin.exists() {
        let ver = std::process::Command::new("curl")
            .args([
                "-fsSL",
                "https://downloads.claude.ai/claude-code-releases/latest",
            ])
            .output()
            .expect("curl --version probe");
        assert!(ver.status.success(), "fetch claude latest version failed");
        let version = String::from_utf8_lossy(&ver.stdout).trim().to_string();
        eprintln!("--- claude CLI version: {version} ---");
        let url =
            format!("https://downloads.claude.ai/claude-code-releases/{version}/linux-x64/claude");
        let dl = std::process::Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&claude_bin)
            .arg(&url)
            .output()
            .expect("spawn claude download");
        assert!(
            dl.status.success(),
            "claude CLI download failed: stdout={} stderr={}",
            String::from_utf8_lossy(&dl.stdout),
            String::from_utf8_lossy(&dl.stderr),
        );
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&claude_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&claude_bin, perms).unwrap();
    }
    (harness_bin, claude_bin)
}

/// Bake a debian-slim rootfs with curl + ca-certs, agentd injected,
/// AND bake the real Claude Code harness pack directly into the
/// rootfs at `/opt/engram/harness/` (ADR 0021 P1.5: harness lives in
/// the rootfs now, not on a separate substrate).
///
/// Dockerfile COPYs the prebuilt harness wrapper + claude CLI from
/// the build context; engram.toml declares the custom-harness
/// launch contract. The egress CA reaches the guest at runtime via
/// `AgentSpec.host_ca_pem`, which triggers an `InstallHostCa` vsock
/// RPC before SpawnHarness.
async fn bake_harness_rootfs(repo: &str, harness_bin: &Path, claude_bin: &Path) -> PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let agent_bin = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("engram-agentd");
    assert!(agent_bin.exists(), "musl agentd missing");

    let src = tempfile::tempdir().expect("source dir");
    // Stage the harness binaries into the Docker build context so
    // COPY can pull them into /opt/engram/harness/. Layout mirrors
    // the published `harness-claude` artifact: `harness` is the
    // engram-harness-claude wrapper, `claude` is the Anthropic
    // Claude Code CLI alongside.
    std::fs::copy(harness_bin, src.path().join("harness")).unwrap();
    std::fs::copy(claude_bin, src.path().join("claude")).unwrap();
    // No in-container network — the dev-vm's Docker daemon has DNS
    // issues during apt-get. debian-slim already has /bin/sh + the
    // base TLS libraries; ca-certificates is wired by agentd's
    // `InstallHostCa` install + `SSL_CERT_FILE` env-var family it
    // exports onto every harness child.
    std::fs::write(
        src.path().join("Dockerfile"),
        "FROM debian:bookworm-slim\n\
         RUN mkdir -p /workspace /opt/engram/harness\n\
         COPY harness /opt/engram/harness/harness\n\
         COPY claude /opt/engram/harness/claude\n\
         RUN chmod +x /opt/engram/harness/harness /opt/engram/harness/claude\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        format!(
            r#"name = "{repo}"

[harness]
name = "claude"
exec = "/opt/engram/harness/harness"
"#,
        ),
    )
    .unwrap();

    let images = tempfile::tempdir().expect("images");
    let images_path = images.path().to_path_buf();
    std::mem::forget(images);
    let chunk_root = tempfile::tempdir().expect("chunk store");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(chunk_root.path().to_path_buf()),
    );
    std::mem::forget(chunk_root);
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);

    let baker = Builder::new(DockerCli::new(), chunk_store);
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: repo.into(),
            tag: "warm-1".into(),
            images_dir: images_path.clone(),
            format: Format::Ext4,
            agent_injection: Some(AgentInjection {
                agent_binary: agent_bin,
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: Transport::Vsock,
                init_script: None,
            }),
            parent_disk_bootstrap_path: None,
            parent_disk_chunks_blob_digest: None,
        })
        .await
        .expect("bake ext4");

    outcome.rootfs_path
}

/// Spin up a real engram-egress-proxy + Registry + SystemResolver
/// (real DNS — points the proxy at the actual api.anthropic.com
/// upstream). Returns the proxy port + the CA PEM (so the bake can
/// inject it).
async fn spawn_real_proxy() -> (u16, String, Arc<engram_egress_proxy::Registry>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let proxy_dir = tempfile::tempdir().expect("proxy dir");
    let proxy_dir_path = proxy_dir.path().to_path_buf();
    std::mem::forget(proxy_dir);
    let ca = Arc::new(engram_egress_proxy::Ca::load_or_generate(&proxy_dir_path).expect("ca gen"));
    let ca_pem = ca.cert_pem.clone();
    let registry = Arc::new(engram_egress_proxy::Registry::new());
    let mint = Arc::new(engram_egress_proxy::CertMint::new(ca.clone()));

    // proxy_port can be anything (we wire it into FirecrackerConfig
    // so host_startup builds matching iptables REDIRECT rules).
    // dns_port MUST be 5353 — that's the DEFAULT_DNS_PORT baked
    // into engram-sandbox-firecracker::net::host_startup_lines's
    // REDIRECT rules. Setting it to anything else means the
    // kernel REDIRECTs DNS to a port no one's listening on and
    // resolution silently times out (caught by an earlier run of
    // this test: iptables UDP/53 → 5353 had 40 packets, proxy was
    // bound on a different port, and Claude CLI never got past
    // DNS).
    let proxy_port: u16 = 28443;
    let dns_port: u16 = engram_sandbox_firecracker::net::DEFAULT_DNS_PORT;
    let proxy_bind: std::net::SocketAddr = format!("0.0.0.0:{proxy_port}").parse().unwrap();
    let dns_bind: std::net::SocketAddr = format!("0.0.0.0:{dns_port}").parse().unwrap();
    let mut proxy_cfg = engram_egress_proxy::ProxyConfig::new(proxy_bind, registry.clone(), mint);
    proxy_cfg.dns_bind_addr = Some(dns_bind);
    // SystemResolver: defer to the host's DNS so api.anthropic.com
    // resolves to its real IP. With a fake-upstream resolver we'd
    // never actually exercise the real-upstream path; that's the
    // whole point of these tests.
    let proxy = engram_egress_proxy::Proxy::new(proxy_cfg);
    tokio::spawn(async move {
        let _ = proxy.run().await;
    });
    sleep(Duration::from_millis(200)).await;
    (proxy_port, ca_pem, registry)
}

async fn wait_for_guest_ip(
    pooled: &PooledBackend,
    id: engram_core::SandboxId,
    deadline: Duration,
) -> String {
    let start = std::time::Instant::now();
    loop {
        if let Some(ip) = pooled.guest_ip(id).await {
            return ip;
        }
        assert!(
            start.elapsed() < deadline,
            "guest_ip never resolved within {deadline:?}",
        );
        sleep(Duration::from_millis(200)).await;
    }
}

/// Set up a HarnessSink that captures events into a shared Vec.
/// Returns the sink + the Vec. Mirrors harness_loopback's sink
/// pattern: handshake (read attach, ack ok) then drain frames.
fn capture_sink() -> (
    engram_core::traits::HarnessSink,
    Arc<Mutex<Vec<engram_harness_proto::HarnessEvent>>>,
) {
    let collected: Arc<Mutex<Vec<engram_harness_proto::HarnessEvent>>> =
        Arc::new(Mutex::new(Vec::new()));
    let collected_for_sink = collected.clone();
    let sink: engram_core::traits::HarnessSink = Arc::new(move |mut stream| {
        let collected = collected_for_sink.clone();
        tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(stream.as_mut());
            let _attach: engram_harness_proto::HarnessAttach =
                match engram_harness_proto::read_msg(&mut reader).await {
                    Ok(a) => a,
                    Err(e) => {
                        eprintln!("--- sink handshake read failed: {e} ---");
                        return;
                    }
                };
            let ack = engram_harness_proto::HarnessAttachAck {
                ok: true,
                message: None,
            };
            if let Err(e) = engram_harness_proto::write_msg(&mut writer, &ack).await {
                eprintln!("--- sink ack write failed: {e} ---");
                return;
            }
            while let Ok(frame) =
                engram_harness_proto::read_msg::<_, engram_harness_proto::HarnessFrame>(&mut reader)
                    .await
            {
                if let engram_harness_proto::HarnessFrame::Event(ev) = frame {
                    eprintln!("--- captured HarnessEvent: {ev:?} ---");
                    collected.lock().push(ev);
                }
            }
        });
    });
    (sink, collected)
}

/// Run one Claude harness session against api.anthropic.com (with a
/// bogus token so the API will 401). Asserts at least one
/// HarnessEvent comes back through the vsock — proves the full
/// chain ran end-to-end.
async fn drive_harness(
    pooled: &PooledBackend,
    sandbox_id: engram_core::SandboxId,
    session_id: engram_core::SessionId,
    ca_pem: &str,
    captured: Arc<Mutex<Vec<engram_harness_proto::HarnessEvent>>>,
) {
    let port = engram_harness_proto::HARNESS_VSOCK_PORT.to_string();
    // ADR 0021 P1.5: harness lives in the rootfs at /opt/engram/harness/
    // (the canonical baked path), not /run/engram/harnesses/claude/
    // (the retired substrate mount).
    let argv = vec![
        "/opt/engram/harness/harness".to_string(),
        "--vsock-host".into(),
        port,
        "--session-id".into(),
        session_id.to_string(),
    ];
    let mut env: HashMap<String, String> = HashMap::new();
    env.insert("ENGRAM_INITIAL_PROMPT".into(), "say hi briefly".into());
    // Bogus token — Claude API will 401. We're not testing API
    // semantics; we're testing the chain works end-to-end. A 401
    // proves the request was issued, the response was received,
    // and Claude CLI parsed it.
    env.insert(
        "CLAUDE_CODE_OAUTH_TOKEN".into(),
        "sk-bogus-e2e-test-token-not-real".into(),
    );
    // PATH so the harness's invocation of curl/etc. finds the
    // bundled `claude` CLI sitting alongside `harness` in the
    // baked dir.
    env.insert(
        "PATH".into(),
        "/opt/engram/harness:/usr/local/bin:/usr/bin:/bin".into(),
    );
    pooled
        .start_agent(
            sandbox_id,
            AgentSpec {
                argv,
                env,
                // ADR 0021 P1.1+P1.2: triggers `InstallHostCa` over
                // vsock right before SpawnHarness, so the in-VM
                // trust store carries the engram proxy CA before
                // the harness's first outbound TLS dial.
                host_ca_pem: Some(ca_pem.to_string()),
            },
        )
        .await
        .expect("start_agent");

    // Wait for the run to terminate, not just start. RunStarted
    // proves bootstrap exec'd the harness and the vsock attach
    // handshake completed — but it doesn't prove the API call
    // round-tripped. RunCompleted proves Claude CLI made its
    // HTTPS call, got a response (401 with our bogus token), and
    // emitted the result back through the harness. That's the
    // full chain.
    //
    // 90s ceiling: bootstrap + bundled-Bun warmup + DNS + TLS
    // handshake + Claude API request + harness emit. The bogus
    // token's 401 response comes back in well under a second once
    // the request is on the wire; the cost is everything before
    // that.
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut got_started = false;
    while std::time::Instant::now() < deadline {
        let evs = captured.lock().clone();
        let mut agent_text: Option<String> = None;
        let mut completed_ok: Option<bool> = None;
        for ev in &evs {
            match ev {
                engram_harness_proto::HarnessEvent::RunStarted { .. } => got_started = true,
                engram_harness_proto::HarnessEvent::RunCompleted { ok, .. } => {
                    completed_ok = Some(*ok);
                }
                engram_harness_proto::HarnessEvent::AgentMessage { text, .. } => {
                    agent_text = Some(text.clone());
                }
                _ => {}
            }
        }
        if let (Some(_ok), Some(text)) = (completed_ok, agent_text.as_ref()) {
            assert!(got_started, "got terminal event without RunStarted");
            // Claude API returns 401 when handed a bogus bearer
            // token. Asserting on the specific shape proves the
            // round-trip ran AND came back from the real upstream
            // — a chain break upstream of the API call would
            // surface as a different error (DNS, TLS, network
            // unreachable) and the assertion would catch it.
            assert!(
                text.contains("401") && text.to_lowercase().contains("bearer"),
                "harness AgentMessage should report the 401 invalid-bearer error from \
                 api.anthropic.com (proving the bogus-token round-trip). Got: {text:?}",
            );
            eprintln!("--- assertion passed: {text:?} ---");
            return;
        }
        sleep(Duration::from_millis(500)).await;
    }
    let evs = captured.lock().clone();
    panic!(
        "harness didn't emit a terminal event (RunCompleted + AgentMessage) within 90s. \
         Got {} events total: {evs:?}. \
         Likely the API call never ran or never got a response — check the proxy logs \
         and the in-VM harness logs.",
        evs.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + KVM + FC + Docker + sudo + internet egress to api.anthropic.com"]
async fn e2e_harness_cold_via_pooled_backend() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_root() {
        return;
    }
    cleanup_host_state();

    let (harness_bin, claude_bin) = ensure_harness_artifacts().await;
    let (proxy_port, ca_pem, registry) = spawn_real_proxy().await;
    let rootfs_path =
        bake_harness_rootfs("engram-e2e-harness-cold", &harness_bin, &claude_bin).await;

    let work = tempfile::tempdir().expect("work");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    cfg.net_pool = Some("10.200.0.0".parse().unwrap());
    cfg.egress_proxy_port = Some(proxy_port);
    let fc = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    fc.host_startup().await.expect("host_startup");
    let pooled = PooledBackend::new(fc.clone() as Arc<dyn SandboxBackend>);

    let (sink, captured) = capture_sink();
    fc.set_harness_sink(sink);

    let spec = SandboxSpec {
        image: "engram-e2e-harness-cold".into(),
        rootfs_source: Some(rootfs_path),
        image_uri: None,
        cpu: CpuLimit { vcpus: 2 },
        memory: MemoryLimit { max_mib: 512 },
        disk: DiskLimit { max_gib: 2 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
    };
    let sandbox_id = pooled.create(spec).await.expect("create");
    let _guest_ip = wait_for_guest_ip(&pooled, sandbox_id, Duration::from_secs(30)).await;

    // Register the session in the proxy registry. For COLD path
    // the guest_ip and vm_internal_ip are the same (no SNAT
    // indirection) so we use guest_ip.
    let session_id = engram_core::SessionId::new();
    let guest_ip: std::net::Ipv4Addr = pooled
        .guest_ip(sandbox_id)
        .await
        .expect("guest_ip")
        .parse()
        .unwrap();
    let allow_list: Vec<String> = ALLOW_HOSTS.iter().map(|s| s.to_string()).collect();
    let network_allow = engram_egress_proxy::HostList::from_manifest(&allow_list, &[]).unwrap();
    registry.register(engram_egress_proxy::SessionState {
        session_id,
        guest_ip,
        network_allow,
        secrets: Vec::new(),
    });

    drive_harness(&pooled, sandbox_id, session_id, &ca_pem, captured).await;

    pooled.destroy(sandbox_id).await.expect("destroy");
    cleanup_host_state();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + KVM + FC + Docker + sudo + internet egress to api.anthropic.com"]
async fn e2e_harness_warm_via_pooled_backend() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_root() {
        return;
    }
    cleanup_host_state();

    let (harness_bin, claude_bin) = ensure_harness_artifacts().await;
    let (proxy_port, ca_pem, registry) = spawn_real_proxy().await;
    let rootfs_path =
        bake_harness_rootfs("engram-e2e-harness-warm", &harness_bin, &claude_bin).await;

    let work = tempfile::tempdir().expect("work");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    cfg.net_pool = Some("10.200.0.0".parse().unwrap());
    cfg.egress_proxy_port = Some(proxy_port);
    let fc = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    fc.host_startup().await.expect("host_startup");
    let pooled = PooledBackend::new(fc.clone() as Arc<dyn SandboxBackend>);

    let (sink, captured) = capture_sink();
    fc.set_harness_sink(sink);

    let spec = SandboxSpec {
        image: "engram-e2e-harness-warm".into(),
        rootfs_source: Some(rootfs_path),
        image_uri: None,
        cpu: CpuLimit { vcpus: 2 },
        memory: MemoryLimit { max_mib: 512 },
        disk: DiskLimit { max_gib: 2 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
    };

    // Cold create → wait → snapshot → destroy → restore (warm netns path).
    let cold_id = pooled.create(spec).await.expect("create");
    let _ = wait_for_guest_ip(&pooled, cold_id, Duration::from_secs(30)).await;
    let metadata = pooled.snapshot(cold_id).await.expect("snapshot");
    pooled.destroy(cold_id).await.expect("destroy cold");

    let warm_id = pooled.restore(metadata).await.expect("restore");
    let _ = wait_for_guest_ip(&pooled, warm_id, Duration::from_secs(30)).await;
    sleep(Duration::from_secs(2)).await;

    // Register WITH the SNAT'd guest_ip (Fix 3 territory). For
    // warm-restored sandboxes guest_ip = snat_cidr.guest() which is
    // exactly what the proxy's peer.ip() sees post-SNAT. Without
    // this matching the registry's key, Registry::lookup misses
    // and the proxy drops the harness's outbound.
    let session_id = engram_core::SessionId::new();
    let guest_ip: std::net::Ipv4Addr = pooled
        .guest_ip(warm_id)
        .await
        .expect("guest_ip")
        .parse()
        .unwrap();
    let allow_list: Vec<String> = ALLOW_HOSTS.iter().map(|s| s.to_string()).collect();
    let network_allow = engram_egress_proxy::HostList::from_manifest(&allow_list, &[]).unwrap();
    registry.register(engram_egress_proxy::SessionState {
        session_id,
        guest_ip,
        network_allow,
        secrets: Vec::new(),
    });

    drive_harness(&pooled, warm_id, session_id, &ca_pem, captured).await;

    pooled.destroy(warm_id).await.expect("destroy warm");
    cleanup_host_state();
}
