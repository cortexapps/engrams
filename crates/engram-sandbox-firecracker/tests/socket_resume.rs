//! ADR 0022 follow-on spike: **does a persistent connection held by a
//! guest process survive an FC snapshot → idle → restore, and HOW does
//! it fail when the underlying connection is gone?**
//!
//! Motivation: a persistent multi-turn `claude` process (warm V8 heap,
//! no Bun re-boot per turn) would cut follow-up latency — but a
//! long-lived process holds network sockets, and an idle→resume freezes
//! the guest while the far peer tears the idle TCP down. The open
//! question is the *failure mode* of reusing that socket after thaw:
//!
//!   - **fast RST** → the client gets `ECONNRESET` immediately and a
//!     retry on a fresh connection works → healing is trivial (lean on
//!     client retry / drop the idle pool pre-snapshot). Best case.
//!   - **black-hole** → the reused socket hangs to its timeout → we'd
//!     need an explicit pre-snapshot quiesce (never reuse across resume).
//!
//! ## Why TCP-DNS to 1.1.1.1:53 (not HTTPS to a real API)
//!
//! The dev VM runs Docker, which sets the filter FORWARD policy to DROP.
//! NO-PROXY mode (`net.rs` §4d) only adds an explicit ACCEPT for the
//! pool → `1.1.1.1:53` (DNS); general `:443` egress relies on a
//! default-ACCEPT FORWARD policy that Docker has overridden, so HTTPS to
//! a real API black-holes on this host. But `1.1.1.1:53` IS reachable —
//! and the FORWARD rule keys on the *whole pool*, not a specific guest
//! IP, so the **restored** VM's fresh IP reaches it too with zero proxy
//! registration. DNS-over-TCP to Cloudflare (which idle-closes TCP per
//! RFC 7766) is a faithful instrument for the gating question: the
//! failure mode of reusing a frozen TCP connection whose peer is gone is
//! decided by the guest's kernel TCP stack + the network path, and is
//! TLS- and client-library-independent (the TCP RST/timeout is what
//! matters; TLS just surfaces it). So this measures the primitive
//! cleanly without fighting Docker's iptables. The egress-proxy path
//! (prod) only *improves* the failure mode (host-local RST instead of a
//! far-peer timeout) and is a separate follow-up run.
//!
//! We freeze via FC **pause → resume on the same VM** (not
//! snapshot→destroy→restore). Reason: restore takes the warm per-VM-netns
//! path, whose no-proxy egress isn't wired on a Docker host, so a
//! restored VM can't reach 1.1.1.1 at all — that black-holes REQ3 and
//! confounds the verdict. pause/resume holds the egress path constant
//! (same TAP/IP), so REQ3_FRESH stays a valid post-resume egress check
//! and REQ2_REUSE is a clean stale-socket measurement. During the pause
//! the guest's TCP timers are frozen while Cloudflare idle-closes the
//! held DNS-TCP connection (RFC 7766, ~seconds), so by REQ2 the reused
//! socket faces a genuinely-gone peer — the same condition a long
//! real-world idle produces. (The socket-reuse failure mode is identical
//! under prod's snapshot→restore: the new-netns SNAT just hands the
//! reused socket a fresh source port, and the far peer RSTs the unknown
//! 5-tuple the same way.)
//!
//! **Intentionally manual-only** (not wired into `run-boot-test.sh all`
//! or CI): it depends on real external egress (`1.1.1.1`), so it's a
//! spike probe, not a hermetic regression gate. Same sudo+KVM gating as
//! `proxy_e2e` (real TAP needs CAP_NET_ADMIN). Run on the dev VM:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release
//! sudo -E env "PATH=$PATH" cargo test -p engram-sandbox-firecracker \
//!     --test socket_resume -- --ignored --nocapture
//! ```
//!
//! Tunable: `ENGRAM_SPIKE_IDLE_SECS` (default 45) — seconds between
//! snapshot and restore (how long the held socket sits dead).

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::ids::SandboxId;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_image_builder::{AgentInjection, BuildRequest, Builder, DockerCli, Format};
use engram_sandbox_firecracker::client::FirecrackerClient;
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};

use common::{drain, fc_preflight, require_bin};

const GUEST_MIB: u32 = 256;

/// The persistent client, baked at `/opt/probe.py`. Raw TCP socket to
/// `1.1.1.1:53` (by IP — no DNS resolution needed) speaking DNS-over-TCP
/// (RFC 7766). The SAME `s` object is held across the freeze and reused
/// in phase 2 — the load-bearing measurement.
const PROBE_PY: &str = r#"#!/usr/bin/env python3
import os, socket, struct, time

DNS = ("1.1.1.1", 53)
LOG = "/tmp/probe.log"
GO  = "/tmp/go"
TMO = 8.0

def log(m):
    with open(LOG, "a") as f:
        f.write(m + "\n")

def query_bytes():
    # DNS A-record query for example.com, RD=1, with the 2-byte TCP
    # length prefix RFC 7766 requires.
    qname = b"\x07example\x03com\x00"
    msg = struct.pack(">HHHHHH", 0x1234, 0x0100, 1, 0, 0, 0) + qname + struct.pack(">HH", 1, 1)
    return struct.pack(">H", len(msg)) + msg

def open_conn():
    return socket.create_connection(DNS, timeout=TMO)

def req(s, tag):
    t = time.time()
    try:
        s.settimeout(TMO)
        s.sendall(query_bytes())
        hdr = s.recv(2)
        dt = time.time() - t
        if len(hdr) < 2:
            log(tag + " EMPTY dt=%.2fs (peer closed, %d bytes)" % (dt, len(hdr)))
            return
        (ln,) = struct.unpack(">H", hdr)
        body = b""
        while len(body) < ln:
            chunk = s.recv(ln - len(body))
            if not chunk:
                break
            body += chunk
        rid = struct.unpack(">H", body[:2])[0] if len(body) >= 2 else -1
        log(tag + " OK dt=%.2fs resp_id=0x%x len=%d" % (dt, rid, len(body)))
    except Exception as e:
        log(tag + " EXC dt=%.2fs %s: %s" % (time.time() - t, type(e).__name__, e))

def main():
    log("START pid=%d" % os.getpid())
    s = None
    try:
        s = open_conn()
        log("CONNECTED")
        req(s, "REQ1")
    except Exception as e:
        log("REQ1 SETUP_EXC %s: %s" % (type(e).__name__, e))
    log("WAITING")
    while not os.path.exists(GO):
        time.sleep(0.5)
    log("GO")
    if s is not None:
        req(s, "REQ2_REUSE")
    try:
        f = open_conn()
        req(f, "REQ3_FRESH")
    except Exception as e:
        log("REQ3_FRESH SETUP_EXC %s: %s" % (type(e).__name__, e))
    log("DONE")

if __name__ == "__main__":
    main()
"#;

/// Delete every `tap-engr-*` device on the host. A failed prior run
/// (e.g. a REQ1 assert that panics before `destroy()`) leaks its TAP;
/// multiple TAPs sharing overlapping `/30` gateways race ARP and kill
/// guest connectivity (→ EHOSTUNREACH). Same guard as `proxy_e2e` /
/// `snapshot_net`. Wipe the slate before each run.
fn delete_stale_taps() {
    let Ok(o) = std::process::Command::new("sh")
        .arg("-c")
        .arg("ip -o link show | awk -F': ' '/tap-engr-/ {print $2}' | awk '{print $1}'")
        .output()
    else {
        return;
    };
    for name in String::from_utf8_lossy(&o.stdout).lines() {
        let name = name.trim();
        if !name.is_empty() {
            let _ = std::process::Command::new("ip")
                .args(["link", "delete", name])
                .output();
        }
    }
}

fn require_root() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    let euid: u32 = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2).and_then(|s| s.parse().ok()))
        .expect("parse euid");
    if euid != 0 {
        eprintln!(
            "SKIP: socket_resume requires root (CAP_NET_ADMIN for TAP). \
             Re-run via `sudo -E env \"PATH=$PATH\" ...`."
        );
        return false;
    }
    true
}

/// Exec `sh -c <cmd>` in the guest, polling exec_stream until agentd is
/// reachable (fresh boot AND restore both need a beat). Returns
/// (stdout, stderr, exit) WITHOUT asserting — the caller decides.
async fn sh(
    backend: &FirecrackerBackend,
    id: SandboxId,
    cmd: &str,
) -> (String, String, Option<i32>) {
    let mut env = HashMap::new();
    env.insert(
        "PATH".to_string(),
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
    );
    let req = ExecRequest {
        command: vec!["/bin/sh".into(), "-c".into(), cmd.into()],
        stdin: None,
        env,
        workdir: None,
        timeout: Some(Duration::from_secs(30)),
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let stream = loop {
        match backend.exec_stream(id, req.clone()).await {
            Ok(s) => break s,
            Err(e) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = e;
            }
            Err(e) => panic!("agent never came up for `{cmd}`: {e:?}"),
        }
    };
    let (out, err, exit) = drain(stream.events).await;
    (
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
        exit,
    )
}

/// Poll `cat /tmp/probe.log` until it contains `needle`, up to `budget`.
/// Returns the full log (whatever we last read).
async fn wait_for_log(
    backend: &FirecrackerBackend,
    id: SandboxId,
    needle: &str,
    budget: Duration,
) -> String {
    let deadline = Instant::now() + budget;
    loop {
        let (last, _, _) = sh(backend, id, "cat /tmp/probe.log 2>/dev/null || true").await;
        if last.contains(needle) || Instant::now() >= deadline {
            return last;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual spike: requires Linux + KVM + firecracker + Docker + sudo + egress to 1.1.1.1"]
async fn persistent_socket_across_snapshot_resume() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("docker") || !require_bin("mke2fs") {
        return;
    }
    if !require_root() {
        return;
    }
    delete_stale_taps();

    let idle_secs: u64 = std::env::var("ENGRAM_SPIKE_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(45);

    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let agent = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("engram-agentd");
    if !agent.exists() {
        eprintln!(
            "SKIP: static-musl engram-agentd not built at {}.\n  \
             cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release",
            agent.display(),
        );
        return;
    }

    // ---- 1. Bake debian-slim + python3 + probe.py ----
    let src = tempfile::tempdir().expect("source dir");
    std::fs::write(
        src.path().join("Dockerfile"),
        "FROM debian:bookworm-slim\n\
         RUN apt-get update && apt-get install -y --no-install-recommends \
             python3 iproute2 && rm -rf /var/lib/apt/lists/*\n\
         COPY probe.py /opt/probe.py\n",
    )
    .unwrap();
    std::fs::write(src.path().join("probe.py"), PROBE_PY).unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        "name = \"engram-socket-resume-spike\"\n",
    )
    .unwrap();

    let images = tempfile::tempdir().expect("images dir");
    let chunk_root = tempfile::tempdir().expect("chunk store root");
    let blob: std::sync::Arc<dyn engram_core::traits::BlobStorage> = std::sync::Arc::new(
        engram_storage_local::LocalBlobStorage::new(chunk_root.path().to_path_buf()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let baker = Builder::new(DockerCli::new(), chunk_store);
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: "engram-socket-resume-spike".into(),
            tag: "warm-1".into(),
            images_dir: images.path().to_path_buf(),
            format: Format::Ext4,
            agent_injection: Some(AgentInjection {
                agent_binary: agent,
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: engram_image_builder::Transport::Vsock,
                init_script: None,
            }),
        })
        .await
        .expect("ext4 bake with agent injection");

    // ---- 2. FC backend, networking ON, NO proxy (1.1.1.1:53 is allowed) ----
    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = Some("10.200.0.0".parse().expect("pool"));
    cfg.egress_proxy_port = None;
    let backend = FirecrackerBackend::new(work.path(), cfg);
    backend
        .host_startup()
        .await
        .expect("host_startup (MASQUERADE + DNS-to-1.1.1.1 rules)");
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    let spec = SandboxSpec {
        image: "engram-socket-resume-spike".into(),
        rootfs_source: Some(outcome.rootfs_path),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: GUEST_MIB },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };
    let vm1 = backend.create(spec).await.expect("create");

    // Dump guest network state for diagnosis, then launch the probe
    // detached (setsid escapes agentd's exec session so it survives the
    // stream closing). It opens REQ1 to 1.1.1.1:53 then idles at WAITING.
    let (lout, lerr, lexit) = sh(
        &backend,
        vm1,
        "ip addr > /tmp/netdiag 2>&1; ip route >> /tmp/netdiag 2>&1; \
         rm -f /tmp/go /tmp/probe.log; \
         setsid python3 /opt/probe.py </dev/null >/tmp/launch.out 2>&1 & \
         echo LAUNCHED",
    )
    .await;
    eprintln!("SPIKE: launch exit={lexit:?} out={lout:?} err={lerr:?}");
    let (ndiag, _, _) = sh(&backend, vm1, "cat /tmp/netdiag 2>/dev/null || true").await;
    eprintln!("SPIKE: --- guest netdiag ---\n{ndiag}\n--- end ---");

    let pre = wait_for_log(&backend, vm1, "WAITING", Duration::from_secs(40)).await;
    eprintln!("SPIKE: --- probe log BEFORE snapshot ---\n{pre}\n--- end ---");
    assert!(
        pre.contains("REQ1 OK"),
        "probe never completed REQ1 to 1.1.1.1:53 — guest egress is broken \
         (route? FORWARD policy? MASQUERADE?). netdiag:\n{ndiag}\nlog:\n{pre}",
    );

    // ---- 3. Pause the VM (freeze guest TCP timers), idle, resume ----
    // pause/resume on the SAME vm holds egress constant (same TAP / IP /
    // root-netns path that REQ1 just proved works), so REQ3_FRESH stays a
    // valid post-resume egress check and REQ2_REUSE is a *clean*
    // stale-socket measurement. (snapshot→destroy→restore takes the warm
    // per-VM-netns path, whose no-proxy egress isn't wired on a Docker
    // host — it black-holes REQ3 and confounds the verdict. The
    // socket-reuse failure mode is identical either way: a frozen guest
    // TCP meeting a peer that closed during the freeze. In prod the
    // new-netns SNAT just gives the reused socket a fresh source port, so
    // the far peer RSTs the unknown 5-tuple the same way.)
    let st = backend.snapshot_state(vm1).expect("vm1 state");
    let api = FirecrackerClient::new(&st.firecracker_socket);
    let t = Instant::now();
    api.pause().await.expect("pause");
    eprintln!(
        "SPIKE: paused in {} ms; idling {idle_secs}s (Cloudflare idle-closes the held DNS-TCP conn)",
        t.elapsed().as_millis()
    );
    tokio::time::sleep(Duration::from_secs(idle_secs)).await;
    let t = Instant::now();
    api.resume().await.expect("resume");
    eprintln!("SPIKE: resumed in {} ms", t.elapsed().as_millis());
    let vm2 = vm1; // same VM — pause/resume, not a fresh restore

    // ---- 4. Poke the held socket (REQ2_REUSE) + a fresh one (REQ3_FRESH) ----
    let (gout, _, gexit) = sh(&backend, vm2, "touch /tmp/go; echo GONE").await;
    eprintln!("SPIKE: signal exit={gexit:?} out={gout:?}");

    let post = wait_for_log(&backend, vm2, "DONE", Duration::from_secs(60)).await;
    eprintln!("SPIKE: --- probe log AFTER restore ---\n{post}\n--- end ---");

    // ---- 5. Verdict (printed; the test only asserts post-resume egress works) ----
    let reuse = post
        .lines()
        .find(|l| l.starts_with("REQ2_REUSE"))
        .unwrap_or("REQ2_REUSE <missing>");
    let fresh = post
        .lines()
        .find(|l| l.starts_with("REQ3_FRESH"))
        .unwrap_or("REQ3_FRESH <missing>");
    eprintln!("SPIKE: VERDICT reuse  => {reuse}");
    eprintln!("SPIKE: VERDICT fresh  => {fresh}");
    eprintln!(
        "SPIKE: interpretation — reuse `EXC ... dt<~1s` = fast RST (healing trivial); \
         reuse `EXC ... dt~8s` = black-hole timeout (need pre-snapshot quiesce); \
         reuse `EMPTY dt<~1s` = clean peer-close on reconnect; \
         reuse `OK` = socket survived intact."
    );

    backend.destroy(vm2).await.expect("destroy restored");

    // The one hard invariant: a FRESH connection after resume must work
    // (proves restore re-provisioned egress). If this fails the whole
    // premise is moot regardless of reuse behavior.
    assert!(
        fresh.starts_with("REQ3_FRESH OK"),
        "post-resume FRESH connection to 1.1.1.1:53 failed — restore did \
         not re-provision working egress. fresh=`{fresh}`\n\nfull log:\n{post}",
    );
}
