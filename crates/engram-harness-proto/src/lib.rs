//! Wire types for the harness ↔ host vsock channel.
//!
//! The "harness" is whatever runs the agent process inside the
//! sandbox: an Engram-aware first-party harness (`engram-harness-noop`
//! for tests; agent-specific adapters in production), or a thin
//! adapter wrapping a vendor agent like Claude Code (`engram-harness-claude`).
//!
//! This is deliberately separate from the existing exec channel
//! (`engram-agentd::proto`). The exec channel is host-driven: the host
//! sends one [`engram_agentd::proto::WireRequest`] per connection,
//! gets back a stream or one-shot reply. The harness channel is
//! _harness_-driven: the agent inside the sandbox dials the host and
//! pushes [`HarnessEvent`]s as it makes tool calls; the host can send
//! [`HarnessCommand`]s back over the same connection (Checkpoint /
//! Shutdown) which the harness handles at safe boundaries.
//!
//! Conversation shape:
//!
//! ```text
//!   harness ──[ HarnessAttach { session_id, harness_version } ]──► host
//!   host    ──[ HarnessAttachAck { ok, message } ]──► harness
//!     (host validates session_id is known; rejects unknown sessions)
//!
//!   harness ──[ HarnessFrame::Event(HarnessEvent::*) ]──► host  (0+ times)
//!   host    ──[ HarnessFrame::Command(HarnessCommand::*) ]──► harness (0+ times)
//!     (full duplex from here on)
//!
//!   <connection closed>
//! ```
//!
//! Framing: same scheme as `engram-agentd::proto` — 4-byte big-endian
//! length prefix + bincode body. Reusing the shape keeps the agent
//! image small (one codec).

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use engram_core::SessionId;

/// Vsock port the in-guest harness dials to reach the host. Distinct
/// from the agentd exec port (1024) so the host can demux at `accept`
/// time. Direction: guest-to-host (host listens on a UDS at
/// `<vsock_uds>_<port>.sock`; guest dials AF_VSOCK CID=2 port=1026).
pub const HARNESS_VSOCK_PORT: u32 = 1026;

/// Single-frame size cap. Same as `engram-agentd::proto::MAX_MSG_BYTES`.
/// `transcript_delta` payloads are typically a few KB (one JSONL line
/// per tool call); 16 MiB gives plenty of headroom for outliers.
pub const MAX_MSG_BYTES: usize = 16 * 1024 * 1024;

/// Reserved env key carrying the harness's working directory from
/// coord → agentd. The cwd rides the *existing* `SpawnHarnessRequest.env`
/// field rather than a new wire field on purpose: the host↔agentd frame
/// is positional bincode (`engram-agentd::proto`) and agentd is baked
/// into the image, so a freshly-deployed host can talk to an *older*
/// agentd inside an already-baked base snapshot. Adding a struct field
/// would risk breaking SpawnHarness for every pre-existing image; an env
/// entry an old agentd simply ignores (harness stays in `/`) and a new
/// agentd honors (`current_dir`). Set by `resolve_harness` from the
/// image manifest's `workdir`; consumed (and stripped from the child
/// env) by `engram-agentd`'s harness supervisor.
pub const HARNESS_CWD_ENV: &str = "ENGRAM_HARNESS_CWD";

/// First frame the harness sends after dialing the host. Identifies
/// which session this connection belongs to. The host validates the
/// session exists and is in a state that accepts harness traffic; on
/// rejection it replies with `HarnessAttachAck { ok: false }` and
/// closes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessAttach {
    pub session_id: SessionId,
    /// Free-form identifier for the harness build (e.g.
    /// `"engram-harness-claude/0.1.0"`). Logged at debug; no semantics.
    pub harness_version: String,
}

/// Host's reply to [`HarnessAttach`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessAttachAck {
    pub ok: bool,
    /// Short human-readable reason on `ok = false`. Plaintext on the
    /// vsock — never include sensitive context.
    pub message: Option<String>,
}

/// One frame on the steady-state full-duplex channel after the
/// handshake. The harness sends `Event`s, the host sends `Command`s.
/// Wrapping in a single enum keeps a single bincode-deserializer at
/// each end.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum HarnessFrame {
    Event(HarnessEvent),
    Command(HarnessCommand),
}

/// Events the harness emits as the agent inside it does work. The
/// host forwards each into `session_events`; consumers (Slack bot,
/// web UI, audit log) read `GET /sessions/:id/events` SSE and render
/// directly from the structured fields below — no per-agent decoder.
///
/// **Shape choices for chat consumers:**
/// - Tool call events carry small structured `args_summary` /
///   `result_summary` strings (≤ a few KB), not the agent's native
///   bytes. Slack / web UIs render them straight.
/// - `AgentMessage` is the assistant's text response between tool
///   calls (or a system message). v1 ships per-final-message —
///   adapters consolidate streaming responses before emitting.
///   `AgentMessageChunk` for live-typing UIs is a future variant.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum HarnessEvent {
    /// New agent run started (typically: user prompt arrived). The
    /// harness assigns `run_id`; subsequent events in this run carry
    /// the same `run_id`. `prompt_summary` is the first ~1 KB of
    /// the prompt for Slack/UI rendering.
    RunStarted {
        run_id: String,
        prompt_summary: Option<String>,
    },
    /// Assistant / user / system text emitted by the agent. v1 is
    /// per-final-message: streaming agents (Claude, OpenCode)
    /// consolidate text content within one logical message before
    /// emitting. `text` is truncated to 64 KB at the wire — bigger
    /// gets clipped with a `…[truncated N bytes]` suffix the adapter
    /// owns.
    AgentMessage {
        run_id: String,
        /// Adapter-local id for de-dup / threading. Often the
        /// underlying agent's message id (Claude's `message.id`,
        /// OpenCode's event ulid).
        message_id: String,
        role: AgentRole,
        text: String,
    },
    /// Tool call beginning. `tool_call_id` ties Started/Completed.
    /// `args_summary` is a human-readable rendering (≤ 1 KB) of the
    /// tool's input — the adapter picks the format (e.g. for Bash:
    /// the command line; for Read: the file path).
    ToolCallStarted {
        run_id: String,
        tool_call_id: String,
        tool_name: String,
        args_summary: Option<String>,
    },
    /// Tool call finished. `result_summary` is a human-readable
    /// rendering (≤ 4 KB) of the tool's output — what a Slack bot
    /// or UI would show after the tool-call header.
    ToolCallCompleted {
        run_id: String,
        tool_call_id: String,
        tool_name: String,
        ok: bool,
        duration_ms: u64,
        result_summary: Option<String>,
    },
    /// Run finished cleanly (or failed terminally). After this, the
    /// adapter MUST emit `Idle` to mark "awaiting user input."
    RunCompleted { run_id: String, ok: bool },
    /// Explicit "I'm awaiting user input." Engram's idle-eviction
    /// soft TTL fires N seconds after this. Adapters MUST emit it
    /// after every `RunCompleted`; emitting redundantly (no run in
    /// between) just resets the soft timer, which is fine but
    /// wasteful — don't do it.
    Idle,
}

/// Who emitted an [`HarnessEvent::AgentMessage`].
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// The agent talking to the user / driving the run.
    Assistant,
    /// User-injected content visible in the agent's view (rare;
    /// some adapters surface the prompt itself this way).
    User,
    /// Adapter / agent-system messages (init banners, errors, etc.).
    System,
}

impl HarnessEvent {
    /// Discriminant string used as the `kind` column in `session_events`.
    /// Stable across coordinator restarts — clients that subscribe by
    /// kind (Slackbot, Web UI) match on these strings.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RunStarted { .. } => "run_started",
            Self::AgentMessage { .. } => "agent_message",
            Self::ToolCallStarted { .. } => "tool_call_started",
            Self::ToolCallCompleted { .. } => "tool_call_completed",
            Self::RunCompleted { .. } => "run_completed",
            Self::Idle => "harness_idle",
        }
    }

    /// Tool-call ID if this event carries one. Useful for callers
    /// that want to correlate Started/Completed pairs (e.g. a Web UI
    /// rendering the agent's play-by-play timeline).
    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::ToolCallStarted { tool_call_id, .. }
            | Self::ToolCallCompleted { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        }
    }
}

/// Commands the host sends to the harness mid-stream. The harness
/// responds at a safe boundary (i.e. between tool calls), not in the
/// middle of an in-flight call.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum HarnessCommand {
    /// Flush any pending transcript writes; ack when the transcript
    /// is durably on disk (so `git add -A && git push` would capture
    /// it). Reason is informational — logged on the host side.
    Checkpoint { reason: CheckpointReason },
    /// Clean shutdown. `grace_secs` is how long the host will wait
    /// for the harness to exit before escalating. Each adapter
    /// translates this into the right signal sequence for its agent
    /// (SIGINT then SIGKILL for Claude Code; whatever for others).
    Shutdown { grace_secs: u32 },
    /// User prompt for the agent — the next run's input. The adapter
    /// either starts a fresh run (if currently Idle) or queues this
    /// for after the in-flight run's `Idle`. Each adapter's
    /// strategy: Claude spawns `claude --resume <id> --print "<text>"`;
    /// OpenCode POSTs to its server. Engram is opaque to the agent's
    /// internal session shape — `text` is just plumbed through.
    Prompt { text: String },
}

/// Why the host is asking for a checkpoint. Logged in `session_events`
/// so operators can tell idle-driven from preemption-driven flushes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointReason {
    /// Host's idle TTL elapsed; we're about to hot-suspend.
    Idle,
    /// Cloud preemption signal received; flush before VM dies.
    Preempt,
    /// Operator invoked `POST /sessions/:id/checkpoint`.
    Manual,
    /// Run finished; opportunistic checkpoint to align workspace
    /// with conversation state.
    RunCompleted,
}

/// Harness's reply to a [`HarnessCommand::Checkpoint`]. `ok = false`
/// means the harness couldn't reach a safe boundary in time; the host
/// should retry or fall through to its escalation path.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointAck {
    pub ok: bool,
    pub message: Option<String>,
}

/// Harness's reply to a [`HarnessCommand::Shutdown`]. Sent after the
/// last `transcript_delta` has been flushed; harness exits its
/// process tree shortly after.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShutdownAck {
    pub ok: bool,
    pub message: Option<String>,
}

// ---- Forge bridge (ADR 0023) -------------------------------------------
//
// A *separate* guest→host channel from the harness one above: the
// in-guest `GIT_ASKPASS` / `engram-pr` helpers dial the host on
// `FORGE_VSOCK_PORT`, send one [`ForgeRequest`], read one
// [`ForgeResponse`], and close. The host validates the per-session
// broker token, forwards to the coordinator's `GitForge`, and replies.
// Same 4-byte-length + bincode framing (`read_msg`/`write_msg`).

/// Vsock port the in-guest forge helper dials (guest→host). Distinct
/// from the harness channel (1026), agentd exec (1024), and agentd
/// ready (1027) ports so the host can demux at accept time.
pub const FORGE_VSOCK_PORT: u32 = 1028;

/// One-shot request from an in-guest forge helper to the host.
/// Authenticated by the per-session credential-broker token (injected
/// into the guest as `ENGRAM_FORGE_TOKEN`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForgeRequest {
    pub session_id: SessionId,
    pub broker_token: String,
    pub op: ForgeOp,
}

/// The forge operation requested. Mirrors the coord's HTTP forge
/// endpoints so the vsock bridge and the loopback path share semantics.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ForgeOp {
    /// Mint a fresh git credential for `host` (optionally scoping the
    /// installation to `owner`). The reply is [`ForgeResponse::Credential`].
    FetchCredential { host: String, owner: Option<String> },
    /// Open a change request (PR/MR). The reply is
    /// [`ForgeResponse::PullRequest`].
    CreatePullRequest {
        repo: String,
        head_branch: String,
        base_branch: String,
        title: String,
        body: String,
        draft: bool,
    },
}

/// The host's reply to a [`ForgeRequest`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ForgeResponse {
    Credential {
        username: String,
        password: String,
    },
    PullRequest {
        url: String,
        id: u64,
        state: String,
    },
    /// Auth failure, unknown session, no forge configured, or a
    /// provider error — `message` is safe to surface to the guest.
    Error {
        message: String,
    },
}

// ---- Artifact upload bridge (ADR 0026) ---------------------------------
//
// Another *separate* guest→host channel: the in-guest `engram-share`
// helper dials the host on `UPLOAD_VSOCK_PORT`, sends one
// [`UploadRequest`] header frame, then streams the **raw file body**
// (exactly `size_bytes` bytes, NOT length-prefixed or per-chunk framed)
// on the same connection, then reads one [`UploadResponse`] frame.
//
// The raw body is deliberately un-framed: the coord pipes it straight
// into `BlobStorage::put_streaming`, so wrapping each chunk in bincode
// only to unwrap it is pure overhead. `size_bytes` from the header is
// the boundary — the host reads exactly that many bytes, then the
// response frame. `MAX_MSG_BYTES` still caps the header/response frames;
// the body is bounded by `MAX_ARTIFACT_BYTES`, which the coord enforces
// while draining (and aborts + deletes the partial object on exceed).
//
// Authenticated by the same per-session broker token as the forge
// bridge (injected as `ENGRAM_UPLOAD_TOKEN`), but the upload path is NOT
// git-gated — every baked image gets it.

/// Vsock port the in-guest `engram-share` helper dials (guest→host).
/// Distinct from agentd exec (1024), harness (1026), agentd ready
/// (1027), and forge (1028) so the host can demux at accept time.
pub const UPLOAD_VSOCK_PORT: u32 = 1029;

/// Hard cap on a single artifact's body, enforced host-side by the
/// coord while draining the stream. 512 MiB gives headroom for short
/// screen recordings; tune as real usage lands. Distinct from
/// `MAX_MSG_BYTES` (which still caps the header/response *frames*).
pub const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

/// Header frame from the in-guest `engram-share` helper to the host,
/// sent before the raw body stream. Authenticated by the per-session
/// broker token (injected into the guest as `ENGRAM_UPLOAD_TOKEN`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UploadRequest {
    pub session_id: SessionId,
    pub broker_token: String,
    pub op: UploadOp,
}

/// The artifact operation requested. An enum for symmetry with
/// [`ForgeOp`] and room for future ops (e.g. delete); v1 has one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum UploadOp {
    /// Share a file that will surface in the session's conversation
    /// history. `ext` is the raw filename extension (no leading dot)
    /// the guest derived — used only to pick a stored object suffix;
    /// the coord re-derives the persisted media type by sniffing the
    /// body's magic bytes and never trusts a guest-supplied MIME.
    /// `size_bytes` is the exact length of the raw body that follows.
    ShareFile {
        ext: String,
        caption: Option<String>,
        size_bytes: u64,
    },
}

/// The host's reply to an [`UploadRequest`] (after the body stream).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum UploadResponse {
    /// Stored. `media_type` is the coord-detected type, `artifact_id`
    /// the server-generated UUID (string form).
    Shared {
        artifact_id: String,
        media_type: String,
        size_bytes: u64,
    },
    /// Auth failure, unknown session, disallowed media type, over the
    /// size cap / quota, or a storage error — `message` is safe to
    /// surface to the guest.
    Error { message: String },
}

// ---- Framing -----------------------------------------------------------

/// Read one length-prefixed bincode frame. Mirrors
/// `engram-agentd::proto::read_msg` so adapters can share a single
/// codec.
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
            format!("harness frame length {len} exceeds MAX_MSG_BYTES ({MAX_MSG_BYTES})"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    bincode::deserialize(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bincode: {e}")))
}

/// Bincode-encode `msg` and write as a length-prefixed frame.
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
                "encoded harness frame {} exceeds MAX_MSG_BYTES ({MAX_MSG_BYTES})",
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

    fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: T) {
        let mut buf = Vec::new();
        let fut = write_msg(&mut buf, &value);
        futures_block_on(fut).unwrap();
        let mut cur = Cursor::new(buf);
        let got: T = futures_block_on(read_msg(&mut cur)).unwrap();
        assert_eq!(got, value);
    }

    fn futures_block_on<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(f)
    }

    #[test]
    fn attach_round_trip() {
        round_trip(HarnessAttach {
            session_id: SessionId::new(),
            harness_version: "engram-harness-noop/0.1.0".into(),
        });
    }

    #[test]
    fn attach_ack_round_trip() {
        round_trip(HarnessAttachAck {
            ok: true,
            message: None,
        });
        round_trip(HarnessAttachAck {
            ok: false,
            message: Some("unknown session".into()),
        });
    }

    #[test]
    fn event_variants_round_trip() {
        round_trip(HarnessFrame::Event(HarnessEvent::RunStarted {
            run_id: "r1".into(),
            prompt_summary: Some("fix the test".into()),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::ToolCallStarted {
            run_id: "r1".into(),
            tool_call_id: "t1".into(),
            tool_name: "Bash".into(),
            args_summary: Some("cargo test".into()),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::AgentMessage {
            run_id: "r1".into(),
            message_id: "m1".into(),
            role: AgentRole::Assistant,
            text: "hello".into(),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::ToolCallCompleted {
            run_id: "r1".into(),
            tool_call_id: "t1".into(),
            tool_name: "Bash".into(),
            ok: true,
            duration_ms: 12345,
            result_summary: Some("done".into()),
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::RunCompleted {
            run_id: "r1".into(),
            ok: true,
        }));
        round_trip(HarnessFrame::Event(HarnessEvent::Idle));
    }

    #[test]
    fn command_variants_round_trip() {
        round_trip(HarnessFrame::Command(HarnessCommand::Checkpoint {
            reason: CheckpointReason::Idle,
        }));
        round_trip(HarnessFrame::Command(HarnessCommand::Checkpoint {
            reason: CheckpointReason::Preempt,
        }));
        round_trip(HarnessFrame::Command(HarnessCommand::Shutdown {
            grace_secs: 5,
        }));
        round_trip(HarnessFrame::Command(HarnessCommand::Prompt {
            text: "do the thing".into(),
        }));
    }

    #[test]
    fn forge_request_response_round_trip() {
        round_trip(ForgeRequest {
            session_id: SessionId::new(),
            broker_token: "tok".into(),
            op: ForgeOp::FetchCredential {
                host: "github.com".into(),
                owner: Some("cortexapps".into()),
            },
        });
        round_trip(ForgeRequest {
            session_id: SessionId::new(),
            broker_token: "tok".into(),
            op: ForgeOp::CreatePullRequest {
                repo: "cortexapps/engrams".into(),
                head_branch: "feat/x".into(),
                base_branch: "main".into(),
                title: "Add x".into(),
                body: String::new(),
                draft: false,
            },
        });
        round_trip(ForgeResponse::Credential {
            username: "x-access-token".into(),
            password: "ghs_x".into(),
        });
        round_trip(ForgeResponse::PullRequest {
            url: "https://github.com/cortexapps/engrams/pull/1".into(),
            id: 1,
            state: "open".into(),
        });
        round_trip(ForgeResponse::Error {
            message: "nope".into(),
        });
    }

    #[test]
    fn upload_request_response_round_trip() {
        round_trip(UploadRequest {
            session_id: SessionId::new(),
            broker_token: "tok".into(),
            op: UploadOp::ShareFile {
                ext: "png".into(),
                caption: Some("the dashboard after my change".into()),
                size_bytes: 4096,
            },
        });
        round_trip(UploadResponse::Shared {
            artifact_id: "0190f3a2c0f17e2cba12".into(),
            media_type: "image/png".into(),
            size_bytes: 4096,
        });
        round_trip(UploadResponse::Error {
            message: "unsupported media type".into(),
        });
    }

    #[test]
    fn agent_message_round_trips_with_unicode() {
        let ev = HarnessEvent::AgentMessage {
            run_id: "r".into(),
            message_id: "m".into(),
            role: AgentRole::Assistant,
            text: "I see — let me check the workspace 🔧".into(),
        };
        let mut buf = Vec::new();
        futures_block_on(write_msg(&mut buf, &ev)).unwrap();
        let mut cur = Cursor::new(buf);
        let got: HarnessEvent = futures_block_on(read_msg(&mut cur)).unwrap();
        match got {
            HarnessEvent::AgentMessage { text, role, .. } => {
                assert_eq!(text, "I see — let me check the workspace 🔧");
                assert_eq!(role, AgentRole::Assistant);
            }
            _ => panic!("variant mismatch"),
        }
    }

    #[test]
    fn read_rejects_oversized_length() {
        let bad = (MAX_MSG_BYTES as u32 + 1).to_be_bytes();
        let mut cur = Cursor::new(bad.to_vec());
        let err = futures_block_on(read_msg::<_, HarnessFrame>(&mut cur)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("MAX_MSG_BYTES"));
    }

    #[test]
    fn event_kind_strings_are_stable() {
        // Persisted clients (Slackbot, audit log) match on these.
        // Changing them is a breaking change — this test forces a
        // deliberate update if anyone tries.
        assert_eq!(
            HarnessEvent::RunStarted {
                run_id: "x".into(),
                prompt_summary: None,
            }
            .kind(),
            "run_started"
        );
        assert_eq!(
            HarnessEvent::ToolCallStarted {
                run_id: "x".into(),
                tool_call_id: "t".into(),
                tool_name: "B".into(),
                args_summary: None,
            }
            .kind(),
            "tool_call_started"
        );
        assert_eq!(
            HarnessEvent::ToolCallCompleted {
                run_id: "x".into(),
                tool_call_id: "t".into(),
                tool_name: "B".into(),
                ok: true,
                duration_ms: 0,
                result_summary: None,
            }
            .kind(),
            "tool_call_completed"
        );
        assert_eq!(
            HarnessEvent::AgentMessage {
                run_id: "x".into(),
                message_id: "m".into(),
                role: AgentRole::Assistant,
                text: "hi".into(),
            }
            .kind(),
            "agent_message"
        );
        assert_eq!(
            HarnessEvent::RunCompleted {
                run_id: "x".into(),
                ok: true,
            }
            .kind(),
            "run_completed"
        );
        assert_eq!(HarnessEvent::Idle.kind(), "harness_idle");
    }

    #[test]
    fn tool_call_id_only_set_for_tool_call_events() {
        assert_eq!(HarnessEvent::Idle.tool_call_id(), None);
        assert_eq!(
            HarnessEvent::RunStarted {
                run_id: "x".into(),
                prompt_summary: None,
            }
            .tool_call_id(),
            None
        );
        let id = "tool-42";
        assert_eq!(
            HarnessEvent::ToolCallStarted {
                run_id: "x".into(),
                tool_call_id: id.into(),
                tool_name: "Read".into(),
                args_summary: None,
            }
            .tool_call_id(),
            Some(id)
        );
    }
}
