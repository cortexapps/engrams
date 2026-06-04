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
use engram_sandbox_firecracker::client::FirecrackerClient;
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
    // The CLI treats 401 as retryable (it clears cached auth and
    // re-attempts, default 10 tries with exponential backoff) — but
    // our 401 is INTENTIONAL, so when that path engages the run
    // takes minutes and blows the 90s deadline below with only
    // RunStarted captured (the historical flake shape of this
    // test). Zero retries makes the first response surface
    // immediately: deterministic, and still proves the full chain.
    env.insert("CLAUDE_CODE_MAX_RETRIES".into(), "0".into());
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
                session_env: HashMap::new(),
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
    // Timed out. Pull the in-VM harness log (wrapper tracing + the
    // claude CLI's stderr both land there) before panicking so the
    // failure is diagnosable from CI output alone — without this the
    // only artifact is "RunStarted then silence".
    let harness_log = match pooled
        .exec(
            sandbox_id,
            engram_core::types::sandbox::ExecRequest {
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "tail -c 16384 /var/log/engram/harness.log 2>&1".into(),
                ],
                stdin: None,
                env: HashMap::new(),
                workdir: None,
                timeout: Some(Duration::from_secs(10)),
            },
        )
        .await
    {
        Ok(h) => String::from_utf8_lossy(&h.stdout).into_owned(),
        Err(e) => format!("<harness.log fetch failed: {e}>"),
    };
    let evs = captured.lock().clone();
    panic!(
        "harness didn't emit a terminal event (RunCompleted + AgentMessage) within 90s. \
         Got {} events total: {evs:?}. \
         Likely the API call never ran or never got a response. \
         In-VM harness.log tail:\n{harness_log}",
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
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 2 },
        memory: MemoryLimit { max_mib: 512 },
        disk: DiskLimit { max_gib: 2 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
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
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 2 },
        memory: MemoryLimit { max_mib: 512 },
        disk: DiskLimit { max_gib: 2 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };

    // Cold create → wait → snapshot → destroy → restore. The settle
    // window that used to live here is now enforced inside
    // `PooledBackend::snapshot` via `inner.wait_agent_ready` — see the
    // comment there for the cold-boot race it guards against.
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

/// ADR 0021 P1.6: `SessionMode::DevVm` against a HARNESSED image. The
/// `/opt/engram/harness/harness` binary (+ `claude` CLI) is baked into
/// the rootfs exactly like `e2e_harness_cold`, but the coord
/// resolution path (`resolve_harness`) returns `None` and passes an
/// **empty-argv** `AgentSpec` to `start_agent`. agentd's
/// `SpawnHarness` handler hits the dedicated readiness-probe branch
/// (`harness_supervisor.rs::spawn`: `if req.argv.is_empty() { return
/// Ok(None) }`) and never execs the harness binary, even though it's
/// sitting right there in the rootfs.
///
/// The coord-level shape is covered by
/// `engram-coordinator/tests/api.rs::create_session_dev_vm_mode_skips_harness_on_harnessed_image`
/// against a mock backend. This is the real-FC counterpart — it
/// proves the whole chain (cold create → `wait_agent_ready` →
/// `InstallHostCa` → empty-argv `SpawnHarness` → Active) survives a
/// real microVM with no harness child running.
///
/// Assertions:
///   1. `start_agent` with empty argv returns `Ok(())` — no spawn
///      failure even though the rootfs has a (would-be-executable)
///      harness binary at `/opt/engram/harness/harness`.
///   2. The harness sink is never invoked — no `HarnessEvent` ever
///      arrives, because agentd didn't spawn the wrapper, so the
///      wrapper never dialled back over vsock.
///   3. The sandbox is genuinely usable as a dev VM after the
///      readiness probe: an in-VM `exec` over vsock round-trips
///      cleanly (proves agentd is alive on the same connection that
///      a SHELL-tab open would use).
///   4. Destroy is clean (no zombie child to reap, no leaked UDS).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + KVM + FC + Docker + sudo"]
async fn e2e_harness_dev_vm_mode_via_pooled_backend() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_root() {
        return;
    }
    cleanup_host_state();

    let (harness_bin, claude_bin) = ensure_harness_artifacts().await;
    let (proxy_port, ca_pem, _registry) = spawn_real_proxy().await;
    // Same rootfs as `e2e_harness_cold` — harness binary is COPY'd in
    // at `/opt/engram/harness/harness`. If start_agent ignored the
    // empty argv and still tried to spawn, the test would surface that
    // as either a spawn failure or a sink event below.
    let rootfs_path =
        bake_harness_rootfs("engram-e2e-harness-devvm", &harness_bin, &claude_bin).await;

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
        image: "engram-e2e-harness-devvm".into(),
        rootfs_source: Some(rootfs_path),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 2 },
        memory: MemoryLimit { max_mib: 512 },
        disk: DiskLimit { max_gib: 2 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };
    let sandbox_id = pooled.create(spec).await.expect("create");
    let _guest_ip = wait_for_guest_ip(&pooled, sandbox_id, Duration::from_secs(30)).await;

    // The DevVm-mode AgentSpec: empty argv (resolve_harness → None),
    // empty env, host_ca_pem still delivered (the dev VM can still
    // outbound through the proxy if the operator wants). This is
    // exactly what coord builds in `sessions.rs:739-745` when
    // `agent_for_session` is None.
    pooled
        .start_agent(
            sandbox_id,
            AgentSpec {
                argv: Vec::new(),
                env: HashMap::new(),
                session_env: HashMap::new(),
                host_ca_pem: Some(ca_pem.clone()),
            },
        )
        .await
        .expect("start_agent with empty argv (readiness probe)");

    // Give any (incorrectly-spawned) harness a moment to land an
    // event in the sink. 1 s is generous — the harness wrapper's
    // own startup ends with a vsock dial; if a wrong impl path
    // execed it, RunStarted (or at minimum the attach handshake)
    // would arrive within ~100 ms. Anything longer is just margin.
    sleep(Duration::from_secs(1)).await;
    let events = captured.lock().clone();
    assert!(
        events.is_empty(),
        "DevVm mode must not spawn the harness — got events: {events:?}",
    );

    // Prove agentd is reachable on the *same* vsock path that
    // start_agent used. `exec` is a `WireRequest::Exec` over
    // ENGRAM_AGENTD_PORT — same connect path as InstallHostCa /
    // SpawnHarness. Success here means the dev VM is fully usable
    // for shell-tab / ad-hoc commands without a harness driving it.
    let exec_handle = pooled
        .exec(
            sandbox_id,
            engram_core::types::sandbox::ExecRequest {
                command: vec!["/bin/sh".into(), "-c".into(), "echo dev-vm-ok".into()],
                stdin: None,
                env: HashMap::new(),
                workdir: None,
                timeout: Some(Duration::from_secs(10)),
            },
        )
        .await
        .expect("exec via agentd over vsock");
    let stdout = String::from_utf8_lossy(&exec_handle.stdout);
    assert!(
        stdout.contains("dev-vm-ok"),
        "exec stdout missing marker: {stdout:?} stderr={:?}",
        String::from_utf8_lossy(&exec_handle.stderr),
    );
    assert_eq!(
        exec_handle.exit_status,
        Some(0),
        "exec exit_status: {:?} stderr={:?}",
        exec_handle.exit_status,
        String::from_utf8_lossy(&exec_handle.stderr),
    );

    // Sanity: still no harness events after the exec round-trip —
    // catches any accidental SpawnHarness fallback that might fire
    // on the second vsock dial.
    let events_after_exec = captured.lock().clone();
    assert!(
        events_after_exec.is_empty(),
        "harness sink received events after dev-VM exec — empty argv probe leaked into a real spawn: {events_after_exec:?}",
    );

    pooled.destroy(sandbox_id).await.expect("destroy");
    cleanup_host_state();
}

// ====================================================================
// ADR 0022 follow-on spike (Phase B): the REAL claude binary, PERSISTENT
// across an idle→resume.
//
// Phase A (engram-sandbox-firecracker::socket_resume) proved the network
// primitive: a frozen TCP connection reused after resume fails with an
// *immediate* RST (not a black-hole). Phase B asks the app-layer
// question with the real Bun/undici stack: if we keep ONE `claude`
// process alive (`--input-format stream-json`, the SDK's persistent
// transport shape) and it holds a pooled connection across a freeze,
// what does the next turn do?
//
// We drive a persistent claude over a /bin/sh FIFO (no python — the bake
// has no apt network): a `sleep` holds the FIFO's write end open so
// claude never EOFs between turns; each turn is one user message line
// `printf`'d into the FIFO. Bogus token + MAX_RETRIES=0 so each turn's
// 401 round-trip is fast and deterministic (same trick as the harness
// tests). Egress is the REAL proxy → real api.anthropic.com (REDIRECT
// sidesteps the dev-vm Docker FORWARD-DROP). We freeze with FC
// pause/resume on the same VM (cold TAP-in-root path, so the proxy
// registration stays valid — no SNAT re-register).
//
// Turn 1 (pre-freeze) warms a pooled connection. Turn 2 (immediately
// post-resume) is load-bearing: undici's keepalive timer is on the
// guest's monotonic clock, which froze during the pause, so undici
// believes ~0 time passed and WILL try to reuse the now-dead pooled
// socket. Turn 3 confirms steady state. We print each turn's stream-json
// output; the test asserts only that claude stays alive and keeps
// producing terminal results across the freeze (the recovery signal).
// ====================================================================

/// Exec `sh -c <cmd>` via the pooled backend; return (stdout, stderr, exit).
async fn exec_sh(
    pooled: &PooledBackend,
    id: engram_core::SandboxId,
    cmd: &str,
) -> (String, String, Option<i32>) {
    let h = pooled
        .exec(
            id,
            engram_core::types::sandbox::ExecRequest {
                command: vec!["/bin/sh".into(), "-c".into(), cmd.into()],
                stdin: None,
                env: HashMap::new(),
                workdir: None,
                timeout: Some(Duration::from_secs(30)),
            },
        )
        .await
        .expect("exec_sh");
    (
        String::from_utf8_lossy(&h.stdout).into_owned(),
        String::from_utf8_lossy(&h.stderr).into_owned(),
        h.exit_status,
    )
}

/// Send one stream-json user turn into the persistent claude's FIFO.
async fn send_turn(pooled: &PooledBackend, id: engram_core::SandboxId, text: &str) {
    let line = format!(r#"{{"type":"user","message":{{"role":"user","content":"{text}"}}}}"#);
    // single-quote the JSON (no single quotes inside) so the shell passes
    // it verbatim into the FIFO.
    let cmd = format!("printf '%s\\n' '{line}' > /tmp/cin");
    let (_o, e, x) = exec_sh(pooled, id, &cmd).await;
    eprintln!("PHASEB: send_turn({text}) exit={x:?} err={e:?}");
}

/// Poll `/tmp/cout` until it holds at least `want` stream-json `result`
/// events (one per completed turn) or `budget` elapses. Returns the cout
/// snapshot.
async fn wait_for_results(
    pooled: &PooledBackend,
    id: engram_core::SandboxId,
    want: usize,
    budget: Duration,
) -> String {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let (cout, _, _) = exec_sh(pooled, id, "cat /tmp/cout 2>/dev/null || true").await;
        let results = cout.matches(r#""type":"result""#).count();
        if results >= want || std::time::Instant::now() >= deadline {
            return cout;
        }
        sleep(Duration::from_millis(1000)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual spike: requires Linux + KVM + FC + Docker + sudo + egress to api.anthropic.com"]
async fn e2e_persistent_claude_socket_across_resume() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_root() {
        return;
    }
    cleanup_host_state();

    // Default short so this is a reasonable CI citizen; bump via
    // ENGRAM_SPIKE_IDLE_SECS for a longer real-world idle when probing.
    let idle_secs: u64 = std::env::var("ENGRAM_SPIKE_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);

    let (harness_bin, claude_bin) = ensure_harness_artifacts().await;
    let (proxy_port, ca_pem, registry) = spawn_real_proxy().await;
    // Reuse the harness bake — it COPYs the real claude CLI to
    // /opt/engram/harness/claude (we drive it directly, ignoring the
    // wrapper).
    let rootfs_path =
        bake_harness_rootfs("engram-persistent-claude", &harness_bin, &claude_bin).await;

    let work = tempfile::tempdir().expect("work");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    cfg.net_pool = Some("10.200.0.0".parse().unwrap());
    cfg.egress_proxy_port = Some(proxy_port);
    let fc = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    fc.host_startup().await.expect("host_startup");
    let pooled = PooledBackend::new(fc.clone() as Arc<dyn SandboxBackend>);

    let spec = SandboxSpec {
        image: "engram-persistent-claude".into(),
        rootfs_source: Some(rootfs_path),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 2 },
        memory: MemoryLimit { max_mib: 512 },
        disk: DiskLimit { max_gib: 2 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };
    let sandbox_id = pooled.create(spec).await.expect("create");
    let guest_ip_str = wait_for_guest_ip(&pooled, sandbox_id, Duration::from_secs(30)).await;

    // Empty-argv start_agent: readiness + InstallHostCa (engram proxy CA
    // into the guest trust store), but NO harness spawn — we drive claude
    // ourselves.
    pooled
        .start_agent(
            sandbox_id,
            AgentSpec {
                argv: Vec::new(),
                env: HashMap::new(),
                session_env: HashMap::new(),
                host_ca_pem: Some(ca_pem.clone()),
            },
        )
        .await
        .expect("start_agent readiness + CA");

    let session_id = engram_core::SessionId::new();
    let guest_ip: std::net::Ipv4Addr = guest_ip_str.parse().unwrap();
    let allow_list: Vec<String> = ALLOW_HOSTS.iter().map(|s| s.to_string()).collect();
    let network_allow = engram_egress_proxy::HostList::from_manifest(&allow_list, &[]).unwrap();
    registry.register(engram_egress_proxy::SessionState {
        session_id,
        guest_ip,
        network_allow,
        secrets: Vec::new(),
    });

    // Write the CA (Bun's NODE_EXTRA_CA_CERTS) + a clean launcher SCRIPT
    // (a quoted heredoc keeps `$!` literal so the real claude PID lands in
    // claude.pid — inline nested-quoting mangled this in an earlier run).
    // `sleep` holds the FIFO write end open so claude never EOFs between
    // turns. The host-side probe confirmed this exact invocation: claude
    // emits system/init → assistant → result and stays alive per turn.
    // IS_SANDBOX=1 is the documented escape hatch for claude's root-check
    // — without it, `--dangerously-skip-permissions` refuses to start as
    // root (which is how the guest runs it). Our real harness sets this;
    // the host-side probe didn't need it (ran as a normal user).
    let run_claude = "#!/bin/sh\n\
        sleep 3600 > /tmp/cin &\n\
        env NODE_EXTRA_CA_CERTS=/tmp/ca.pem CLAUDE_CODE_OAUTH_TOKEN=sk-bogus-persistent-spike \
            CLAUDE_CODE_MAX_RETRIES=0 IS_SANDBOX=1 PATH=/opt/engram/harness:/usr/bin:/bin \
            /opt/engram/harness/claude --input-format stream-json --output-format stream-json \
            --verbose --dangerously-skip-permissions < /tmp/cin > /tmp/cout 2>&1 &\n\
        echo $! > /tmp/claude.pid\n\
        wait\n";
    let setup = format!(
        "cat > /tmp/ca.pem <<'CAEOF'\n{ca_pem}\nCAEOF\n\
         cat > /tmp/run-claude.sh <<'RCEOF'\n{run_claude}RCEOF\n\
         mkfifo /tmp/cin 2>/dev/null; rm -f /tmp/cout /tmp/claude.pid; \
         setsid sh /tmp/run-claude.sh </dev/null >/tmp/launch.out 2>&1 & echo LAUNCHED"
    );
    let (lo, le, lx) = exec_sh(&pooled, sandbox_id, &setup).await;
    eprintln!("PHASEB: launch exit={lx:?} out={lo:?} err={le:?}");
    // Diagnostic: if claude fails to start, launch.out carries its stderr.
    let (diag, _, _) = exec_sh(
        &pooled,
        sandbox_id,
        "sleep 2; echo '--- launch.out ---'; cat /tmp/launch.out 2>/dev/null; \
         echo '--- claude.pid ---'; cat /tmp/claude.pid 2>/dev/null",
    )
    .await;
    eprintln!("PHASEB: post-launch diag:\n{diag}");

    async fn alive(pooled: &PooledBackend, id: engram_core::SandboxId) -> String {
        let (o, _, _) = exec_sh(
            pooled,
            id,
            "kill -0 $(cat /tmp/claude.pid 2>/dev/null) 2>/dev/null && echo ALIVE || echo DEAD",
        )
        .await;
        o.trim().to_string()
    }

    // ---- Turn 1 (pre-freeze): warm a pooled connection ----
    send_turn(&pooled, sandbox_id, "ping one").await;
    let c1 = wait_for_results(&pooled, sandbox_id, 1, Duration::from_secs(75)).await;
    eprintln!("PHASEB: --- cout after turn 1 ---\n{c1}\n--- end ---");
    eprintln!(
        "PHASEB: claude alive after turn1 = {}",
        alive(&pooled, sandbox_id).await
    );

    // ---- Freeze: pause / idle / resume (same VM) ----
    let st = fc.snapshot_state(sandbox_id).expect("snapshot_state");
    let api = FirecrackerClient::new(&st.firecracker_socket);
    api.pause().await.expect("pause");
    eprintln!("PHASEB: paused; idling {idle_secs}s");
    sleep(Duration::from_secs(idle_secs)).await;
    api.resume().await.expect("resume");
    eprintln!(
        "PHASEB: resumed; claude alive = {}",
        alive(&pooled, sandbox_id).await
    );

    // ---- Turn 2 (post-resume): reuses the now-stale pooled socket ----
    send_turn(&pooled, sandbox_id, "ping two").await;
    let c2 = wait_for_results(&pooled, sandbox_id, 2, Duration::from_secs(75)).await;
    eprintln!("PHASEB: --- cout after turn 2 ---\n{c2}\n--- end ---");

    // ---- Turn 3 (post-resume): steady-state recovery ----
    send_turn(&pooled, sandbox_id, "ping three").await;
    let c3 = wait_for_results(&pooled, sandbox_id, 3, Duration::from_secs(75)).await;
    eprintln!("PHASEB: --- cout after turn 3 ---\n{c3}\n--- end ---");

    let results = c3.matches(r#""type":"result""#).count();
    let alive_final = alive(&pooled, sandbox_id).await;
    eprintln!(
        "PHASEB: VERDICT — total result events across 3 turns = {results}; claude alive_final = {alive_final}"
    );
    eprintln!(
        "PHASEB: interpretation — 3 results + ALIVE = persistent claude survived the freeze and \
         every turn round-tripped (connection recovered transparently). \
         <3 results or DEAD = claude couldn't continue across resume (note which turn stalled)."
    );

    pooled.destroy(sandbox_id).await.expect("destroy");
    cleanup_host_state();

    // CI gate: a persistent claude must survive the freeze and round-trip
    // all three turns (1 pre-freeze + 2 post-resume). <3 results or a dead
    // process means the connection didn't recover across idle->resume.
    assert!(
        results >= 3,
        "expected 3 result events (all turns round-tripped across pause/resume), got {results}\n{c3}",
    );
    assert_eq!(
        alive_final, "ALIVE",
        "persistent claude did not survive the freeze",
    );
}

// ====================================================================
// ADR 0037 P1 e2e: the REWRITTEN harness drives a PERSISTENT claude
// across MULTIPLE turns and an FC pause/resume — the load-bearing new
// behavior nothing else covers (the cold/warm tests above are
// single-turn). Runs in CI via the test-firecracker job's
// `--test e2e_harness --run-ignored ignored-only`.
//
// Unlike `drive_harness` (one prompt via ENGRAM_INITIAL_PROMPT), this
// sends a SECOND prompt over the harness command channel after turn 1
// completes, with an FC pause/resume in between, and asserts TWO
// RunCompleted events on ONE persistent harness/claude (no respawn, no
// reconnect — pause/resume freezes the guest but keeps the host vsock).
// Bogus token → each turn 401s, which is fine: we're testing the
// harness turn-loop + persistence, not API semantics.
// ====================================================================

/// Like `capture_sink` but also returns a sender the test can use to
/// push `HarnessCommand`s (e.g. a follow-up `Prompt`) down the harness
/// connection. The sink task multiplexes reading events + forwarding
/// commands on the one stream. The command receiver is taken on the
/// first connection (pause/resume keeps a single connection, so there's
/// no reconnect to contend with).
fn capture_sink_with_sender() -> (
    engram_core::traits::HarnessSink,
    Arc<Mutex<Vec<engram_harness_proto::HarnessEvent>>>,
    tokio::sync::mpsc::Sender<engram_harness_proto::HarnessCommand>,
) {
    let collected: Arc<Mutex<Vec<engram_harness_proto::HarnessEvent>>> =
        Arc::new(Mutex::new(Vec::new()));
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<engram_harness_proto::HarnessCommand>(8);
    let cmd_rx_slot = Arc::new(Mutex::new(Some(cmd_rx)));
    let collected_for_sink = collected.clone();
    let cmd_rx_for_sink = cmd_rx_slot.clone();
    let sink: engram_core::traits::HarnessSink = Arc::new(move |mut stream| {
        let collected = collected_for_sink.clone();
        let mut cmd_rx = cmd_rx_for_sink.lock().take();
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
            if let Err(e) = engram_harness_proto::write_msg(
                &mut writer,
                &engram_harness_proto::HarnessAttachAck {
                    ok: true,
                    message: None,
                },
            )
            .await
            {
                eprintln!("--- sink ack write failed: {e} ---");
                return;
            }
            loop {
                tokio::select! {
                    frame = engram_harness_proto::read_msg::<_, engram_harness_proto::HarnessFrame>(&mut reader) => {
                        match frame {
                            Ok(engram_harness_proto::HarnessFrame::Event(ev)) => {
                                eprintln!("--- captured HarnessEvent: {ev:?} ---");
                                collected.lock().push(ev);
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    cmd = async {
                        match cmd_rx.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending::<Option<engram_harness_proto::HarnessCommand>>().await,
                        }
                    } => {
                        match cmd {
                            Some(c) => {
                                let _ = engram_harness_proto::write_msg(
                                    &mut writer,
                                    &engram_harness_proto::HarnessFrame::Command(c),
                                )
                                .await;
                            }
                            None => cmd_rx = None,
                        }
                    }
                }
            }
        });
    });
    (sink, collected, cmd_tx)
}

/// Count `RunCompleted` events in the capture buffer, polling until
/// `want` is reached or `budget` elapses. Returns the final count.
async fn wait_run_completed(
    captured: &Arc<Mutex<Vec<engram_harness_proto::HarnessEvent>>>,
    want: usize,
    budget: Duration,
) -> usize {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let n = captured
            .lock()
            .iter()
            .filter(|e| matches!(e, engram_harness_proto::HarnessEvent::RunCompleted { .. }))
            .count();
        if n >= want || std::time::Instant::now() >= deadline {
            return n;
        }
        sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + KVM + FC + Docker + sudo + internet egress to api.anthropic.com"]
async fn e2e_harness_multiturn_across_resume() {
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
        bake_harness_rootfs("engram-e2e-harness-multiturn", &harness_bin, &claude_bin).await;

    let work = tempfile::tempdir().expect("work");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    cfg.net_pool = Some("10.200.0.0".parse().unwrap());
    cfg.egress_proxy_port = Some(proxy_port);
    let fc = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    fc.host_startup().await.expect("host_startup");
    let pooled = PooledBackend::new(fc.clone() as Arc<dyn SandboxBackend>);

    let (sink, captured, cmd_tx) = capture_sink_with_sender();
    fc.set_harness_sink(sink);

    let spec = SandboxSpec {
        image: "engram-e2e-harness-multiturn".into(),
        rootfs_source: Some(rootfs_path),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 2 },
        memory: MemoryLimit { max_mib: 512 },
        disk: DiskLimit { max_gib: 2 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };
    let sandbox_id = pooled.create(spec).await.expect("create");
    let guest_ip: std::net::Ipv4Addr = wait_for_guest_ip(&pooled, sandbox_id, Duration::from_secs(30))
        .await
        .parse()
        .unwrap();

    let session_id = engram_core::SessionId::new();
    let allow_list: Vec<String> = ALLOW_HOSTS.iter().map(|s| s.to_string()).collect();
    let network_allow = engram_egress_proxy::HostList::from_manifest(&allow_list, &[]).unwrap();
    registry.register(engram_egress_proxy::SessionState {
        session_id,
        guest_ip,
        network_allow,
        secrets: Vec::new(),
    });

    // ---- Turn 1 via the harness (ENGRAM_INITIAL_PROMPT) ----
    let port = engram_harness_proto::HARNESS_VSOCK_PORT.to_string();
    let argv = vec![
        "/opt/engram/harness/harness".to_string(),
        "--vsock-host".into(),
        port,
        "--session-id".into(),
        session_id.to_string(),
    ];
    let mut env_map: HashMap<String, String> = HashMap::new();
    env_map.insert("ENGRAM_INITIAL_PROMPT".into(), "turn one — say hi".into());
    env_map.insert(
        "CLAUDE_CODE_OAUTH_TOKEN".into(),
        "sk-bogus-multiturn-test".into(),
    );
    env_map.insert("CLAUDE_CODE_MAX_RETRIES".into(), "0".into());
    env_map.insert(
        "PATH".into(),
        "/opt/engram/harness:/usr/local/bin:/usr/bin:/bin".into(),
    );
    pooled
        .start_agent(
            sandbox_id,
            AgentSpec {
                argv,
                env: env_map,
                session_env: HashMap::new(),
                host_ca_pem: Some(ca_pem.clone()),
            },
        )
        .await
        .expect("start_agent");

    let n1 = wait_run_completed(&captured, 1, Duration::from_secs(90)).await;
    assert!(n1 >= 1, "turn 1 never completed (got {n1} RunCompleted)");
    eprintln!("MULTITURN: turn 1 complete ({n1} RunCompleted)");

    // ---- FC pause/resume: freeze the guest (harness + its persistent
    //      claude child) and thaw it. The host vsock connection
    //      persists (pause is a vCPU freeze, not a teardown), so the
    //      harness stays attached — no reconnect. ----
    let st = fc.snapshot_state(sandbox_id).expect("snapshot_state");
    let api = FirecrackerClient::new(&st.firecracker_socket);
    api.pause().await.expect("pause");
    eprintln!("MULTITURN: paused; idling 5s");
    sleep(Duration::from_secs(5)).await;
    api.resume().await.expect("resume");
    eprintln!("MULTITURN: resumed");

    // ---- Turn 2 over the command channel: must run on the SAME
    //      persistent claude child (no respawn). ----
    cmd_tx
        .send(engram_harness_proto::HarnessCommand::Prompt {
            text: "turn two — say bye".into(),
        })
        .await
        .expect("send turn-2 prompt");

    let n2 = wait_run_completed(&captured, 2, Duration::from_secs(90)).await;

    // Diagnostics before asserting.
    let events = captured.lock().clone();
    let started = events
        .iter()
        .filter(|e| matches!(e, engram_harness_proto::HarnessEvent::RunStarted { .. }))
        .count();
    let completed = events
        .iter()
        .filter(|e| matches!(e, engram_harness_proto::HarnessEvent::RunCompleted { .. }))
        .count();
    eprintln!("MULTITURN: after turn 2 — RunStarted={started} RunCompleted={completed}");

    pooled.destroy(sandbox_id).await.expect("destroy");
    cleanup_host_state();

    // The load-bearing assertion: TWO turns round-tripped on ONE
    // persistent harness across a pause/resume. If the rewrite spawned
    // a fresh claude per prompt, or the child didn't survive the freeze,
    // or the `result`-frame turn boundary mis-fired, we'd see < 2.
    assert!(
        n2 >= 2,
        "expected >=2 RunCompleted (two turns on one persistent child across pause/resume), got {n2}",
    );
    assert!(
        started >= 2,
        "expected >=2 RunStarted (one per turn), got {started}",
    );
}
