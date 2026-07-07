//! Wire protocol for host ↔ in-guest agent.
//!
//! Both ends are Rust and we own both schemas; the wire format is
//! `4-byte big-endian length` + bincode-encoded body. Bincode (1.x)
//! plugs into the existing serde derives, encodes `Vec<u8>` byte-for-
//! byte (vs JSON which inflates 3-5×), and is fast enough that the
//! agent doesn't burn CPU on encoding inside a 128 MiB guest VM.
//!
//! ## Conversation shape
//!
//! After the optional handshake (see below), the host sends one
//! [`WireRequest`] per connection. The verb's response shape
//! depends on the variant:
//!
//! - `WireRequest::Exec(req)` — agent streams [`WireExecEvent`]s
//!   ending with `Exit`. Same as the original exec-only protocol;
//!   the new envelope just wraps it.
//! - `WireRequest::Stat | Upload | Download | Ping | Shutdown` —
//!   agent sends exactly one [`WireResponse`] and closes.
//!
//! ### Without auth (development, default for back-compat):
//!
//! ```text
//!   host ──[ WireRequest::Exec(WireExecRequest) ]──► agent
//!   agent ──[ WireExecEvent::Stdout(bytes) ]──► host    (0+ times)
//!   agent ──[ WireExecEvent::Stderr(bytes) ]──► host    (0+ times)
//!   agent ──[ WireExecEvent::Exit(status)  ]──► host    (exactly once)
//!   <connection closed>
//! ```
//!
//! For non-streaming verbs:
//!
//! ```text
//!   host ──[ WireRequest::Stat { path } ]──► agent
//!   agent ──[ WireResponse::Stat(WireStatResponse) ]──► host
//!   <connection closed>
//! ```
//!
//! ### With first-frame token auth (production, when the agent is
//! started with `--token <T>` or finds `engram_token=<T>` on the
//! kernel cmdline):
//!
//! ```text
//!   host ──[ WireHandshake { token, agent_version } ]──► agent
//!   agent ──[ WireHandshakeAck { ok, message } ]──► host
//!     (if !ok, agent closes; host treats as auth failure)
//!   host ──[ WireRequest::* ]──► agent
//!     ... same as above ...
//! ```
//!
//! Ordering across stdout/stderr is best-effort: the agent fans out
//! both readers into a single mpsc channel. Within a single stream
//! ordering is preserved; across streams it isn't (which matches what
//! the kernel itself guarantees on tty interleaving anyway).

use std::collections::HashMap;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Maximum size of one framed message (16 MiB). Caps adversarial
/// inputs and accidental stdout-bomb bugs without being so tight that
/// a real `cargo build` log line truncates.
pub const MAX_MSG_BYTES: usize = 16 * 1024 * 1024;

/// ADR 0015 M1: agentd dials the host on this port at startup,
/// once its RPC listener is bound, to signal "I'm ready." The host
/// blocks on `accept()` here in `start_agent` — no poll, no
/// timeout-then-retry. Replaces the boot-race CONNECT-then-retry
/// dance the host used to run against port 1024.
pub const ENGRAM_AGENTD_READY_PORT: u32 = 1027;

/// Wire frame agentd writes to the ready-port stream on startup.
/// The presence of the frame is the readiness signal; the body is
/// purely informational (logged on the host side for debugging /
/// version-skew detection).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentReady {
    /// Free-form version string for the agent build. Logged at info;
    /// no semantics.
    pub agent_version: String,
}

/// One exec request, wire-encoded as the *first* frame the host sends
/// after connecting to the agent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireExecRequest {
    /// argv. `command[0]` is the program; never invokes a shell.
    pub command: Vec<String>,
    /// Optional bytes piped to the child's stdin before EOF. Use
    /// `Some(Vec::new())` to close stdin immediately; `None` to leave
    /// stdin connected to /dev/null.
    pub stdin: Option<Vec<u8>>,
    /// Env vars layered onto the child's environment (added, not
    /// replaced). The agent inherits its own env first.
    pub env: HashMap<String, String>,
    /// `current_dir` for the child. `None` keeps the agent's cwd.
    pub workdir: Option<String>,
    /// Wall-clock timeout. After this, the agent SIGKILLs the child
    /// and emits `Exit(None)`. `None` disables the timeout.
    pub timeout_ms: Option<u64>,
}

/// Each event the agent emits during exec. The stream is terminated by
/// exactly one `Exit` (or an abrupt connection close on agent crash,
/// which the host treats as a failure).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum WireExecEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// `None` = signalled / timed out. `Some(0)` = clean exit 0.
    Exit(Option<i32>),
}

/// First frame the host sends when auth is enabled. The agent
/// validates `token` against the value it loaded at startup
/// (`--token <T>` CLI arg or `engram_token=<T>` on the kernel
/// cmdline) and replies with [`WireHandshakeAck`]. `agent_version`
/// is purely informational — the agent logs it so a deployment-
/// version skew between host and guest is visible in agent logs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireHandshake {
    pub token: String,
    pub agent_version: String,
}

/// Agent's response to [`WireHandshake`]. On `ok = true` the agent
/// continues to read the [`WireRequest`] frame as in the no-auth
/// flow. On `ok = false` the agent closes the connection after
/// sending the ack — `message` carries a single short reason the
/// host can surface in logs (NEVER include the expected token or
/// any guess at the supplied one — the message is plaintext on
/// the vsock, not a secrets channel).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireHandshakeAck {
    pub ok: bool,
    pub message: Option<String>,
}

/// Multi-verb request envelope. The host sends exactly one of these
/// per connection after the (optional) handshake; the agent
/// dispatches and either streams (Exec) or sends a single
/// [`WireResponse`] (everything else). Single-frame size cap is
/// [`MAX_MSG_BYTES`] (16 MiB) per the existing framing layer; for
/// Upload / Download that bounds payload size at 16 MiB. Multi-
/// frame chunked uploads are a follow-up.
///
/// **APPEND-ONLY.** The wire is bincode, which encodes enums by
/// variant *index*. agentd is baked into session images / base
/// snapshots, so the host-agent routinely speaks to agentds built
/// from older trees. Inserting a variant mid-enum shifts every
/// later index and desyncs that pair. New variants go at the END of
/// the enum — same rule for [`WireResponse`].
///
/// 2026-07 core-ops fold: the former standalone CA-install verb was
/// deleted (its payload now rides the
/// [`SpawnHarness`](Self::SpawnHarness) frame as
/// [`SpawnHarnessRequest::host_ca_pem`]); `Sync`/`StartBrowser`/
/// `StopBrowser` renumbered down one index each. Every agentd baked
/// into an existing image/base snapshot is incompatible with this
/// build — a zero-user clean break, not a tombstoned variant.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum WireRequest {
    /// Run a command in the sandbox and stream its output. Wraps
    /// the original [`WireExecRequest`] one-to-one — back-compat
    /// preserved by callers updating to `WireRequest::Exec(req)`.
    Exec(WireExecRequest),
    /// Read filesystem metadata for `path` inside the sandbox.
    /// Returns [`WireResponse::Stat`] always — `exists = false`
    /// instead of an error if the path is missing, to keep
    /// "does this exist" cheap to ask.
    Stat { path: String },
    /// Write `bytes` to `path` inside the sandbox, creating parent
    /// directories as needed. `mode` is the unix file mode to
    /// `chmod` to after the write (mostly for `+x` on uploaded
    /// scripts); `None` keeps the OS default.
    Upload {
        path: String,
        bytes: Vec<u8>,
        mode: Option<u32>,
    },
    /// Read the entire contents of `path` from the sandbox into
    /// the response. Errors with `WireResponse::Error` if the
    /// file doesn't exist or exceeds the framing cap.
    Download { path: String },
    /// Liveness probe. Agent replies [`WireResponse::Pong`].
    Ping,
    /// Ask the agent to exit cleanly after replying. The agent
    /// sends [`WireResponse::ShutdownAck`] then closes; main.rs's
    /// accept loop notices the agent task ended and exits the
    /// process. Used for graceful VM shutdown coordination.
    Shutdown,
    /// Ask the agent for its primary IPv4 address. Used by the host
    /// to discover the guest's DHCP-assigned address on backends
    /// that route traffic by IP (the VZ NAT bridge), so the
    /// coordinator can proxy a WebSocket to a TCP service running
    /// inside the guest (e.g. ttyd for the in-browser shell).
    /// Replies [`WireResponse::GuestIp`] with `None` if no eligible
    /// non-loopback address could be determined.
    GuestIp,
    /// Ensure `ttyd` is running and bound to `port` (defaults to
    /// 7681). On first call after VM boot the agent spawns ttyd; on
    /// subsequent calls it checks the existing handle is still
    /// alive and the port is accepting, restarting only if not.
    /// Either way the agent only replies once a fresh TCP connect
    /// to the loopback succeeds — so the host can dial ttyd with
    /// confidence right after this returns.
    ///
    /// This decouples the in-browser shell from any timing
    /// assumption about the warm snapshot: ttyd no longer needs to
    /// be in the snapshot, and even if it is, the agent re-probes
    /// before declaring it ready.
    ///
    /// Replies [`WireResponse::ShellReady`] on success, or
    /// [`WireResponse::Error`] if the spawn or the port probe
    /// fails (no ttyd binary, kernel refused, port held by
    /// something else, etc.).
    StartShell {
        /// Optional port override. `None` → 7681.
        port: Option<u16>,
    },
    /// Spawn (or respawn) the session's harness child inside the
    /// guest. The agent owns a single harness child at a time; a
    /// fresh `SpawnHarness` call kills any prior child before
    /// launching the new one — the host uses that for clean
    /// re-attach after FC snapshot/restore.
    ///
    /// ADR 0021 P1.4: argv points at a path inside the rootfs (the
    /// image's `[harness] exec`, typically `/opt/engram/harness/...`).
    /// No drive mount; agentd just exec's argv.
    ///
    /// 2026-07 core-ops fold: this frame also carries the per-host
    /// egress-proxy CA (`host_ca_pem`, ADR 0021 P1) — folded in from
    /// the former standalone CA-install verb. agentd installs it (if
    /// present) BEFORE spawning, including on the empty-argv
    /// readiness probe below, so a dev_vm session with no harness
    /// still trusts the proxy. Install failure replies
    /// [`WireResponse::Error`] and does not spawn — a harness
    /// without the proxy CA fails every outbound TLS dial opaquely,
    /// so this fails loud instead. Idempotent: the agent caches the
    /// last installed PEM (zero-I/O on a same-cert resume) and
    /// reports whether it changed via
    /// [`WireResponse::HarnessSpawned::ca_changed`].
    ///
    /// Empty argv is a **readiness probe**: the agent skips the
    /// spawn (after installing the CA, if any) and replies
    /// `HarnessSpawned { pid: None, .. }` — useful for callers that
    /// want to confirm agentd is reachable on vsock, or deliver the
    /// CA, without launching anything (the no-harness / dev-VM
    /// path).
    ///
    /// Replies [`WireResponse::HarnessSpawned`] on success or
    /// [`WireResponse::Error`] if the CA install or the spawn fails.
    SpawnHarness(SpawnHarnessRequest),
    /// Flush the guest's filesystem buffers to the virtio-blk disk.
    /// Replies [`WireResponse::Synced`] once `sync(2)` returns.
    ///
    /// Used by clone-snapshot backends (VZ) before they pause + clone
    /// the rootfs: those backends capture only on-disk state (cold-boot
    /// restore, no memory image), so any write still sitting in the
    /// guest's page cache would be lost from the snapshot. Flushing
    /// first makes the clone capture the guest's just-written state.
    /// FC doesn't need this — its memory snapshot carries the dirty
    /// pages — so only the console/VZ path sends it. (Sent to an older
    /// baked agentd that predates the variant, the decode fails and the
    /// host's flush degrades to best-effort/logged — by design.)
    Sync,
    /// Ensure the in-guest browser stack (Xvfb + openbox + chromium +
    /// x11vnc) is running and x11vnc is bound to `port` (defaults to
    /// [`crate::browser::DEFAULT_VNC_PORT`], 5900). Lazy + idempotent,
    /// exactly like [`Self::StartShell`]: on first call the agent spawns
    /// the `engram-browser` launcher (shipped + PATH-symlinked by the
    /// `browser` bundle, ADR 0065); on later calls it re-probes and respawns
    /// only if the stack went away. The agent only replies once a fresh TCP
    /// connect to the loopback `port` succeeds, so the host's `proxy_vnc`
    /// dial finds a listener right after this returns.
    ///
    /// Replies [`WireResponse::BrowserReady`] on success, or
    /// [`WireResponse::Error`] if the spawn or the port probe fails
    /// (no launcher on PATH, x11vnc never bound, etc.).
    /// Appended last: see the APPEND-ONLY note above.
    StartBrowser {
        /// Optional port override. `None` → 5900.
        port: Option<u16>,
    },
    /// Tear down the browser stack: the agent `killpg`s the launcher's
    /// process group so Xvfb/openbox/chromium/x11vnc all reap together.
    /// Idempotent — a no-op when nothing is running. Replies
    /// [`WireResponse::BrowserStopped`].
    /// Appended last: see the APPEND-ONLY note above.
    StopBrowser,
    /// ADR 0080: adopt the agentd bundle generation now attached at the
    /// reserved agentd slot (`AuxRoDrive::AGENTD_SLOT_INDEX`). Sent by
    /// the host once per **fresh-create restore**, after resume and
    /// before any session state binds. The agent re-mounts the bundle
    /// mounts (the ADR 0035 §3 dance — the host may have `patch_drive`d
    /// the slot in the paused window), compares the slot's
    /// `agentd.sha256` stamp against the one it booted from
    /// (`/run/engram/agentd.sha256`, written by the stage-1 init), and:
    ///
    /// - match → replies [`WireResponse::AgentRefreshed`]
    ///   `{ restarting: false }`. The steady state: one stat.
    /// - mismatch → stages the slot's binary over its tmpfs copy
    ///   (temp + rename; the running inode is untouched), replies
    ///   `{ restarting: true }`, flushes, and **`execv`s itself** (PID 1
    ///   exec — same argv/env). The caller must re-poll readiness
    ///   (`Ping`) before proceeding; the connection drops at exec.
    ///
    /// This is how an agentd roll reaches new sessions with zero image
    /// recapture: the bundle publish swaps the slot's content, the
    /// captured agentd re-execs onto it at the next create. Sent to an
    /// older baked agentd that predates the variant, the decode fails
    /// into the typed skew `Error` above and the host degrades to the
    /// captured agentd (warn, never a failed create) — by design.
    /// Appended last: see the APPEND-ONLY note above.
    RefreshAgent,
}

/// Body of [`WireRequest::SpawnHarness`]. ADR 0021 P1.4 dropped the
/// pre-0021 `harness_dev` / `harness_mount` fields — the harness
/// binary lives in the rootfs now, agentd just exec's `argv`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnHarnessRequest {
    /// argv to exec as the harness child. Empty = readiness probe;
    /// no spawn happens, the agent replies `HarnessSpawned { pid:
    /// None }`. agentd still records `session_env` on a readiness
    /// probe — dev_vm sessions never spawn a harness but their
    /// exec/shell processes still inherit the session env.
    pub argv: Vec<String>,
    /// Harness-only extras (initial prompt, dial address, working-dir
    /// key, forge broker token), merged on top of `session_env` for
    /// the harness child. Additive; duplicate keys take this value.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// The durable session environment (image `[env]` + resolved
    /// secrets + `ENGRAM_SESSION_ID`). agentd holds this from the bind
    /// and applies it as the base env for every process it spawns —
    /// the harness, `/exec` commands, and the interactive shell — so
    /// they all see the same environment. Forge tokens and other
    /// per-request/harness-only vars ride `env` instead.
    #[serde(default)]
    pub session_env: HashMap<String, String>,
    /// Per-host egress-proxy CA (ADR 0021 P1), PEM-encoded.
    /// `None`/empty = no install (tests, deploys without egress
    /// proxying). Installed by agentd BEFORE the harness spawn — and
    /// also on the empty-argv readiness probe, so dev_vm sessions
    /// (no harness) still trust the proxy. ADR 0021 P1 reference:
    /// [`reference_e2b_ca_cert_pattern`] in user memory captures the
    /// E2B `cacerts.go` install model this mirrors. Trailing field —
    /// the only wire-safe struct evolution — added by the 2026-07
    /// core-ops fold of the former standalone CA-install verb.
    #[serde(default)]
    pub host_ca_pem: Option<String>,
}

/// Single-shot response for non-streaming [`WireRequest`] verbs.
/// `Exec` doesn't get one — its response is the stream of
/// [`WireExecEvent`]s.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum WireResponse {
    Stat(WireStatResponse),
    /// Successful upload. No payload — the host knows what it
    /// sent.
    UploadOk,
    Download(WireDownloadResponse),
    Pong,
    ShutdownAck,
    /// Reply to [`WireRequest::GuestIp`]. `None` if the agent
    /// could not determine a non-loopback address (e.g. networking
    /// not configured, all interfaces down).
    GuestIp(Option<String>),
    /// Reply to [`WireRequest::StartShell`]. ttyd is alive AND a
    /// TCP probe to `127.0.0.1:port` from inside the VM completed
    /// successfully — when the host dials the guest IP on this
    /// same port immediately afterward, it should find a listener.
    /// `spawned` is true if this call started ttyd, false if it
    /// was already running and only re-probed.
    ShellReady {
        port: u16,
        spawned: bool,
    },
    /// Reply to [`WireRequest::SpawnHarness`]. `pid` is the spawned
    /// child's PID when argv was non-empty; `None` when the call
    /// was a readiness probe (empty argv) or when the spawn
    /// otherwise yielded no child (no-op case). `ca_changed` mirrors
    /// the `changed` flag from the former standalone CA-install
    /// verb's ack: `None` when the request carried no `host_ca_pem`;
    /// `Some(true)` when the agent wrote new bytes (first install or
    /// rotation); `Some(false)` when the PEM matched the one already
    /// installed (zero-I/O resume hot path). Trailing field, added
    /// by the 2026-07 core-ops fold.
    HarnessSpawned {
        pid: Option<u32>,
        #[serde(default)]
        ca_changed: Option<bool>,
    },
    /// Anything the agent couldn't fulfil. `message` is a short
    /// human-readable reason; `kind` mirrors the std `io::ErrorKind`
    /// stringly so the host can map back to a typed error
    /// without a wire-format tied to the unstable enum.
    Error {
        kind: String,
        message: String,
    },
    /// Reply to [`WireRequest::Sync`] — `sync(2)` has returned, so the
    /// guest's dirty page cache is now on the virtio-blk disk.
    /// Appended last: see the APPEND-ONLY note on [`WireRequest`].
    Synced,
    /// Reply to [`WireRequest::StartBrowser`]. x11vnc is alive AND a TCP
    /// probe to `127.0.0.1:port` from inside the VM completed successfully —
    /// when the host dials the guest IP on this same port immediately
    /// afterward, it should find a listener. `spawned` is true if this call
    /// started the stack, false if it was already running and only re-probed.
    /// Appended last: see the APPEND-ONLY note on [`WireRequest`].
    ///
    /// `cdp_warning` (issue #569) is `Some` when x11vnc came up but
    /// chromium's CDP debug port never answered within budget
    /// (dead/crash-looping chrome) — diagnostic only, never fails the RPC.
    /// DELIBERATE wire break (2026-07, #569): this field was added to the
    /// existing variant in place. bincode structs are positional, so a host
    /// built after this change fails to decode a `BrowserReady` from an
    /// agentd baked before it (the recv surfaces as the typed "version skew"
    /// error; remedy: re-bake + RefreshImage). Accepted as a clean break:
    /// the browser path is already broken on pre-#567/#569 bakes, and that
    /// fix train re-bakes every image anyway.
    BrowserReady {
        port: u16,
        spawned: bool,
        cdp_warning: Option<String>,
    },
    /// Reply to [`WireRequest::StopBrowser`] — the browser stack has been
    /// torn down (or there was nothing running).
    /// Appended last: see the APPEND-ONLY note on [`WireRequest`].
    BrowserStopped,
    /// Reply to [`WireRequest::RefreshAgent`]. `restarting = false`: the
    /// running agentd already matches the attached bundle's stamp (or no
    /// agentd bundle is mounted — logged in-guest, never an error).
    /// `restarting = true`: the reply is the agent's last act before it
    /// `execv`s the staged binary — the caller re-polls readiness.
    /// `sha256` is the content stamp the agent runs (post-exec, when
    /// restarting) — `None` when no stamp could be determined.
    /// Appended last: see the APPEND-ONLY note on [`WireRequest`].
    AgentRefreshed {
        restarting: bool,
        sha256: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireStatResponse {
    pub exists: bool,
    pub size: u64,
    /// Modification time in seconds since the Unix epoch, or 0
    /// when the platform doesn't expose one. Useful for "did this
    /// file change since I last looked".
    pub mtime_unix: i64,
    pub is_dir: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireDownloadResponse {
    pub bytes: Vec<u8>,
}

// ---- Framing -----------------------------------------------------------

/// Read one length-prefixed frame off `r` and bincode-decode it.
pub async fn read_msg<R, T>(r: &mut R) -> std::io::Result<T>
where
    R: AsyncReadExt + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MSG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds MAX_MSG_BYTES ({MAX_MSG_BYTES})"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    bincode::deserialize(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bincode: {e}")))
}

/// Bincode-encode `msg` and write it as a length-prefixed frame.
pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let body = bincode::serialize(msg).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bincode: {e}"))
    })?;
    if body.len() > MAX_MSG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "encoded frame {} exceeds MAX_MSG_BYTES ({MAX_MSG_BYTES})",
                body.len()
            ),
        ));
    }
    let len = (body.len() as u32).to_be_bytes();
    w.write_all(&len).await?;
    w.write_all(&body).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn req() -> WireExecRequest {
        WireExecRequest {
            command: vec!["echo".into(), "hello".into()],
            stdin: None,
            env: HashMap::from([("RUST_LOG".into(), "info".into())]),
            workdir: Some("/tmp".into()),
            timeout_ms: Some(5_000),
        }
    }

    #[tokio::test]
    async fn write_then_read_round_trips_request() {
        let mut buf = Vec::new();
        write_msg(&mut buf, &req()).await.unwrap();
        let mut cur = Cursor::new(buf);
        let got: WireExecRequest = read_msg(&mut cur).await.unwrap();
        assert_eq!(got, req());
    }

    #[tokio::test]
    async fn write_then_read_round_trips_each_event_variant() {
        for ev in [
            WireExecEvent::Stdout(b"hello\n".to_vec()),
            WireExecEvent::Stderr(vec![0xff, 0x00, 0xff]),
            WireExecEvent::Exit(Some(0)),
            WireExecEvent::Exit(Some(137)),
            WireExecEvent::Exit(None),
        ] {
            let mut buf = Vec::new();
            write_msg(&mut buf, &ev).await.unwrap();
            let mut cur = Cursor::new(buf);
            let got: WireExecEvent = read_msg(&mut cur).await.unwrap();
            assert_eq!(got, ev);
        }
    }

    #[tokio::test]
    async fn read_truncated_length_prefix_errors() {
        let mut cur = Cursor::new(vec![0u8; 2]); // < 4 bytes
        let err = read_msg::<_, WireExecEvent>(&mut cur).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn read_rejects_oversized_length() {
        // Forge a length prefix of MAX+1 with no body. Catches a
        // malicious / corrupt peer before we vec![0; HUGE].
        let bad_len = (MAX_MSG_BYTES as u32 + 1).to_be_bytes();
        let mut cur = Cursor::new(bad_len.to_vec());
        let err = read_msg::<_, WireExecEvent>(&mut cur).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("MAX_MSG_BYTES"));
    }

    #[tokio::test]
    async fn read_rejects_garbage_body() {
        // 4-byte length says "1 byte" but it's not valid bincode for a
        // WireExecEvent. Should error InvalidData rather than panic.
        let mut buf = vec![0, 0, 0, 1, 0xff];
        let mut cur = Cursor::new(std::mem::take(&mut buf));
        let err = read_msg::<_, WireExecEvent>(&mut cur).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn binary_stdout_round_trips_byte_for_byte() {
        // The whole point of switching from JSON to bincode: a non-UTF-8
        // stdout chunk survives unmolested.
        let chunk: Vec<u8> = (0..=255u8).cycle().take(8 * 1024).collect();
        let ev = WireExecEvent::Stdout(chunk.clone());
        let mut buf = Vec::new();
        write_msg(&mut buf, &ev).await.unwrap();
        let mut cur = Cursor::new(buf);
        let WireExecEvent::Stdout(got) = read_msg(&mut cur).await.unwrap() else {
            panic!("wrong variant");
        };
        assert_eq!(got, chunk);
    }
}
