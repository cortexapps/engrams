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

/// Vsock port the in-guest `engram-bootstrap` listener binds. The
/// host's `start_agent` connects to this port and pushes a
/// [`BootstrapLaunch`] frame; bootstrap reads it and `exec`s the
/// described argv (with merged env) so the per-session agent process
/// can take over. Direction: host-to-guest (host writes
/// `CONNECT 1025\n` to `<vsock_uds>`).
pub const BOOTSTRAP_VSOCK_PORT: u32 = 1025;

/// Single-byte readiness marker the in-VM bootstrap supervisor writes
/// to its accepted stream before reading the [`BootstrapLaunch`]
/// frame. The host's `start_agent` reads this byte first, then writes
/// the launch — guaranteeing the guest is actually consuming bytes
/// when the launch arrives. On virtio-console (VZ), the host's UDS
/// pump accepts a dial immediately (the listener is bound at VM-
/// config time), but bytes the host writes before the guest's port
/// is open get dropped by VZ rather than queued. The byte's value
/// is arbitrary; the host just checks for "one byte received".
pub const BOOTSTRAP_READY_BYTE: u8 = 0xEB;

/// Wire shape for the bootstrap-launch frame. The host sends this
/// once after CONNECTing to [`BOOTSTRAP_VSOCK_PORT`]; bootstrap reads
/// it, prepares the env, optionally mounts the harness device, and
/// `exec`s `argv[0]` with the rest as arguments. Bootstrap exits
/// (via exec replacing its image) — there is no reply.
///
/// Argv may reference any binary baked into the rootfs (typically
/// `/sbin/engram-harness-noop` for dev or `/sbin/engram-harness-claude`
/// for production). Env is merged on top of bootstrap's existing env;
/// duplicate keys take the BootstrapLaunch value.
///
/// ADR 0014 M1.12 (option D): when `harness_dev` is set, bootstrap
/// `mount(2)`s that block device at `harness_mount` (read-only ext4)
/// before exec'ing argv. This lets warm-pool templates be harness-
/// agnostic: the template snapshot captures a stub harness drive,
/// and per-session `PATCH /drives` swaps in the session's chosen
/// harness ext4. Cold-path sessions also use bootstrap-side mount
/// now (engram-init no longer touches the harness); the wire shape
/// is the same.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BootstrapLaunch {
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// `Some(path)` to ask bootstrap to mount that block device
    /// before exec; `None` (or pre-M1.12 frames) skips the mount.
    /// Typical value: `/dev/vdb`.
    #[serde(default)]
    pub harness_dev: Option<String>,
    /// Where to mount `harness_dev`. Required when `harness_dev` is
    /// set; ignored otherwise. Typical value:
    /// `/run/engram/harnesses`.
    #[serde(default)]
    pub harness_mount: Option<String>,
}

/// Single-frame size cap. Same as `engram-agentd::proto::MAX_MSG_BYTES`.
/// `transcript_delta` payloads are typically a few KB (one JSONL line
/// per tool call); 16 MiB gives plenty of headroom for outliers.
pub const MAX_MSG_BYTES: usize = 16 * 1024 * 1024;

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

    /// ADR 0014 M1.12: option-D extension to BootstrapLaunch adds
    /// `harness_dev` + `harness_mount` Option<String> fields. Lock
    /// in the wire shape with serde defaults so an older host
    /// sending a frame without the new fields still deserializes
    /// cleanly on a new bootstrap, AND a new host's frame
    /// (carrying the fields) round-trips correctly.
    #[test]
    fn bootstrap_launch_round_trip_with_harness_fields() {
        round_trip(BootstrapLaunch {
            argv: vec!["/sbin/engram-harness-claude".into(), "--session".into()],
            env: [("ANTHROPIC_API_KEY".to_string(), "sk-test".to_string())]
                .iter()
                .cloned()
                .collect(),
            harness_dev: Some("/dev/vdb".into()),
            harness_mount: Some("/run/engram/harnesses".into()),
        });
        // No-harness variant (kind = none sessions).
        round_trip(BootstrapLaunch {
            argv: vec!["/bin/sleep".into(), "infinity".into()],
            env: Default::default(),
            harness_dev: None,
            harness_mount: None,
        });
    }

    /// Backwards compat: a frame serialized in the pre-M1.12 shape
    /// (argv + env only) MUST deserialize into the new struct with
    /// `harness_dev` + `harness_mount` defaulting to None. Without
    /// `#[serde(default)]` on the new fields this would fail.
    #[test]
    fn bootstrap_launch_deserializes_pre_m1_12_frame() {
        // The legacy shape is just two fields. We synthesize what
        // an older bootstrap binary would have read by constructing
        // a JSON of the old shape and round-tripping through
        // bincode-compatible serde to confirm Option<String> with
        // serde(default) tolerates absent fields.
        //
        // Strategy: serialize a struct shaped like pre-M1.12, then
        // try to deserialize as the new BootstrapLaunch. Use
        // `serde_json` for the round-trip since bincode's
        // sequence-driven format can't add optional fields without
        // a version tag. JSON is the operationally-relevant test
        // because the wire format here is bincode but the *concept*
        // of "old client sends to new server" is what we care about
        // — and bincode would just truncate, which serde(default)
        // handles trivially.
        #[derive(Serialize)]
        struct LegacyShape {
            argv: Vec<String>,
            env: std::collections::HashMap<String, String>,
        }
        let legacy = LegacyShape {
            argv: vec!["/sbin/old-harness".into()],
            env: Default::default(),
        };
        let json = serde_json::to_string(&legacy).unwrap();
        let new: BootstrapLaunch = serde_json::from_str(&json).unwrap();
        assert_eq!(new.argv, vec!["/sbin/old-harness".to_string()]);
        assert!(new.env.is_empty());
        assert!(new.harness_dev.is_none());
        assert!(new.harness_mount.is_none());
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
