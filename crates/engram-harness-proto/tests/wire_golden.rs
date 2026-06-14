//! Byte-level golden + variant-index pins for the harness vsock channel
//! and the forge / upload bridge wire types.
//!
//! ## Why this exists
//!
//! `bincode` (1.x, the framing in `engram-harness-proto`) is positional:
//! enums encode by `u32` variant *index*, structs by field *order*.
//! Neither is self-describing. The pre-existing round-trip tests in
//! `src/lib.rs` cannot catch a format break — both ends recompile
//! together in CI, so a reordered variant or an inserted field
//! round-trips green and then desyncs against a peer built from an older
//! tree. The in-guest harness is baked into session images / base
//! snapshots, so the host routinely speaks to a harness built from an
//! older tree: that skew is real and long-lived.
//!
//! This test pins the exact bytes (`golden/<name>.bin`) plus, for every
//! enum, the `u32` variant index in `bytes[0..4]`. Reordering a variant
//! or adding a non-trailing field fails here with a message naming the
//! evolution rule, turning a silent fleet-desync into a CI failure.
//!
//! ## Regenerating the corpus (only when you INTENTIONALLY evolve a type)
//!
//! Adding a *trailing* enum variant or a *trailing* struct field is the
//! only wire-safe evolution. After such a change, regenerate:
//!
//! ```text
//!   cargo test -p engram-harness-proto --test wire_golden -- --ignored regen_golden
//! ```
//!
//! then `git add` the changed `golden/*.bin` and review the diff: an
//! EXISTING golden file changing bytes is a RED FLAG (you broke the wire
//! for an old baked harness); only NEW files are expected. Reordering or
//! inserting is never OK.

use std::path::PathBuf;

use engram_core::SessionId;
use engram_harness_proto::{
    AgentRole, CheckpointReason, ForgeOp, ForgeResponse, HarnessCommand, HarnessEvent,
    HarnessFrame, UploadOp, UploadResponse,
};
use serde::Serialize;
use uuid::Uuid;

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.bin"))
}

/// A fixed `SessionId` so payloads carrying one encode deterministically.
fn fixed_session_id() -> SessionId {
    SessionId::from(Uuid::from_bytes([
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
        0xef,
    ]))
}

fn assert_golden<T>(name: &str, value: &T)
where
    T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let encoded = bincode::serialize(value).expect("bincode encode");
    let path = golden_path(name);
    let golden = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden file {}: {e}\n\
             regenerate with: cargo test -p engram-harness-proto --test wire_golden -- --ignored regen_golden",
            path.display()
        )
    });
    assert_eq!(
        encoded, golden,
        "wire format for `{name}` changed: bincode output != golden bytes.\n\
         bincode is POSITIONAL — a reordered field/variant or an inserted \
         (non-trailing) field breaks the host↔harness channel for every \
         harness baked into an existing image / base snapshot.\n\
         If this is an intentional, wire-SAFE evolution (a TRAILING \
         field/variant only), regenerate the corpus per the module header."
    );
    let decoded: T = bincode::deserialize(&golden).expect("bincode decode golden");
    assert_eq!(
        &decoded, value,
        "golden bytes for `{name}` no longer decode to the expected value"
    );
}

fn assert_variant_index<T: Serialize>(value: &T, idx: u32, variant: &str) {
    let encoded = bincode::serialize(value).expect("bincode encode");
    assert!(
        encoded.len() >= 4,
        "enum encoding too short for `{variant}`"
    );
    assert_eq!(
        &encoded[0..4],
        &idx.to_le_bytes(),
        "variant index for `{variant}` is not {idx}.\n\
         Enum variants crossing the bincode wire are APPEND-ONLY: a new \
         variant goes at the END so existing indices never shift. \
         Reordering/inserting desyncs every harness built from an older tree."
    );
}

// ---- canonical sample values (deterministic) ---------------------------

fn ev_run_started() -> HarnessEvent {
    HarnessEvent::RunStarted {
        run_id: "r1".into(),
        prompt_summary: Some("fix the test".into()),
    }
}
fn ev_agent_message() -> HarnessEvent {
    HarnessEvent::AgentMessage {
        run_id: "r1".into(),
        message_id: "m1".into(),
        role: AgentRole::Assistant,
        text: "hello".into(),
    }
}
fn ev_tool_call_started() -> HarnessEvent {
    HarnessEvent::ToolCallStarted {
        run_id: "r1".into(),
        tool_call_id: "t1".into(),
        tool_name: "Bash".into(),
        args_summary: Some("cargo test".into()),
    }
}
fn ev_tool_call_completed() -> HarnessEvent {
    HarnessEvent::ToolCallCompleted {
        run_id: "r1".into(),
        tool_call_id: "t1".into(),
        tool_name: "Bash".into(),
        ok: true,
        duration_ms: 12345,
        result_summary: Some("done".into()),
    }
}
fn ev_run_completed() -> HarnessEvent {
    HarnessEvent::RunCompleted {
        run_id: "r1".into(),
        ok: true,
    }
}
fn ev_run_interrupted() -> HarnessEvent {
    HarnessEvent::RunInterrupted {
        run_id: "r1".into(),
    }
}

fn cmd_checkpoint() -> HarnessCommand {
    HarnessCommand::Checkpoint {
        reason: CheckpointReason::Idle,
    }
}
fn cmd_shutdown() -> HarnessCommand {
    HarnessCommand::Shutdown { grace_secs: 5 }
}
fn cmd_prompt() -> HarnessCommand {
    HarnessCommand::Prompt {
        text: "do the thing".into(),
    }
}

// ---- tests -------------------------------------------------------------

#[test]
fn harness_frame_golden_and_variant_indices() {
    // The steady-state frame wrapper: `Event` is index 0, `Command` is 1.
    let event_frame = HarnessFrame::Event(ev_run_started());
    let command_frame = HarnessFrame::Command(cmd_prompt());
    assert_golden("frame_event", &event_frame);
    assert_golden("frame_command", &command_frame);
    assert_variant_index(&event_frame, 0, "HarnessFrame::Event");
    assert_variant_index(&command_frame, 1, "HarnessFrame::Command");
}

#[test]
fn harness_event_golden_and_variant_indices() {
    assert_golden("event_run_started", &ev_run_started());
    assert_golden("event_agent_message", &ev_agent_message());
    assert_golden("event_tool_call_started", &ev_tool_call_started());
    assert_golden("event_tool_call_completed", &ev_tool_call_completed());
    assert_golden("event_run_completed", &ev_run_completed());
    assert_golden("event_run_interrupted", &ev_run_interrupted());
    assert_golden("event_idle", &HarnessEvent::Idle);

    assert_variant_index(&ev_run_started(), 0, "HarnessEvent::RunStarted");
    assert_variant_index(&ev_agent_message(), 1, "HarnessEvent::AgentMessage");
    assert_variant_index(&ev_tool_call_started(), 2, "HarnessEvent::ToolCallStarted");
    assert_variant_index(
        &ev_tool_call_completed(),
        3,
        "HarnessEvent::ToolCallCompleted",
    );
    assert_variant_index(&ev_run_completed(), 4, "HarnessEvent::RunCompleted");
    assert_variant_index(&ev_run_interrupted(), 5, "HarnessEvent::RunInterrupted");
    assert_variant_index(&HarnessEvent::Idle, 6, "HarnessEvent::Idle");
}

#[test]
fn agent_role_golden_and_variant_indices() {
    assert_golden("agent_role_assistant", &AgentRole::Assistant);
    assert_golden("agent_role_user", &AgentRole::User);
    assert_golden("agent_role_system", &AgentRole::System);
    assert_variant_index(&AgentRole::Assistant, 0, "AgentRole::Assistant");
    assert_variant_index(&AgentRole::User, 1, "AgentRole::User");
    assert_variant_index(&AgentRole::System, 2, "AgentRole::System");
}

#[test]
fn harness_command_golden_and_variant_indices() {
    assert_golden("command_checkpoint", &cmd_checkpoint());
    assert_golden("command_shutdown", &cmd_shutdown());
    assert_golden("command_prompt", &cmd_prompt());
    assert_golden("command_interrupt", &HarnessCommand::Interrupt);

    assert_variant_index(&cmd_checkpoint(), 0, "HarnessCommand::Checkpoint");
    assert_variant_index(&cmd_shutdown(), 1, "HarnessCommand::Shutdown");
    assert_variant_index(&cmd_prompt(), 2, "HarnessCommand::Prompt");
    assert_variant_index(&HarnessCommand::Interrupt, 3, "HarnessCommand::Interrupt");
}

#[test]
fn checkpoint_reason_golden_and_variant_indices() {
    assert_golden("checkpoint_reason_idle", &CheckpointReason::Idle);
    assert_golden("checkpoint_reason_preempt", &CheckpointReason::Preempt);
    assert_golden("checkpoint_reason_manual", &CheckpointReason::Manual);
    assert_golden(
        "checkpoint_reason_run_completed",
        &CheckpointReason::RunCompleted,
    );
    assert_variant_index(&CheckpointReason::Idle, 0, "CheckpointReason::Idle");
    assert_variant_index(&CheckpointReason::Preempt, 1, "CheckpointReason::Preempt");
    assert_variant_index(&CheckpointReason::Manual, 2, "CheckpointReason::Manual");
    assert_variant_index(
        &CheckpointReason::RunCompleted,
        3,
        "CheckpointReason::RunCompleted",
    );
}

#[test]
fn forge_op_golden_and_variant_indices() {
    let fetch = ForgeOp::FetchCredential {
        host: "github.com".into(),
        owner: Some("cortexapps".into()),
    };
    let create_pr = ForgeOp::CreatePullRequest {
        repo: "cortexapps/engrams".into(),
        head_branch: "feat/x".into(),
        base_branch: "main".into(),
        title: "Add x".into(),
        body: String::new(),
        draft: false,
    };
    assert_golden("forge_op_fetch_credential", &fetch);
    assert_golden("forge_op_create_pull_request", &create_pr);
    assert_variant_index(&fetch, 0, "ForgeOp::FetchCredential");
    assert_variant_index(&create_pr, 1, "ForgeOp::CreatePullRequest");
}

#[test]
fn forge_response_golden_and_variant_indices() {
    let cred = ForgeResponse::Credential {
        username: "x-access-token".into(),
        password: "ghs_x".into(),
    };
    let pr = ForgeResponse::PullRequest {
        url: "https://github.com/cortexapps/engrams/pull/1".into(),
        id: 1,
        state: "open".into(),
    };
    let err = ForgeResponse::Error {
        message: "nope".into(),
    };
    assert_golden("forge_response_credential", &cred);
    assert_golden("forge_response_pull_request", &pr);
    assert_golden("forge_response_error", &err);
    assert_variant_index(&cred, 0, "ForgeResponse::Credential");
    assert_variant_index(&pr, 1, "ForgeResponse::PullRequest");
    assert_variant_index(&err, 2, "ForgeResponse::Error");
}

#[test]
fn upload_op_golden_and_variant_indices() {
    let share = UploadOp::ShareFile {
        ext: "png".into(),
        caption: Some("the dashboard after my change".into()),
        size_bytes: 4096,
    };
    assert_golden("upload_op_share_file", &share);
    assert_variant_index(&share, 0, "UploadOp::ShareFile");
}

#[test]
fn upload_response_golden_and_variant_indices() {
    let shared = UploadResponse::Shared {
        artifact_id: "0190f3a2c0f17e2cba12".into(),
        media_type: "image/png".into(),
        size_bytes: 4096,
    };
    let err = UploadResponse::Error {
        message: "unsupported media type".into(),
    };
    assert_golden("upload_response_shared", &shared);
    assert_golden("upload_response_error", &err);
    assert_variant_index(&shared, 0, "UploadResponse::Shared");
    assert_variant_index(&err, 1, "UploadResponse::Error");
}

#[test]
fn handshake_and_bridge_payload_structs_golden() {
    use engram_harness_proto::{ForgeRequest, HarnessAttach, HarnessAttachAck, UploadRequest};
    // Structs (no variant index) that ride the wire — pin field order.
    assert_golden(
        "harness_attach",
        &HarnessAttach {
            session_id: fixed_session_id(),
            harness_version: "engram-harness-noop/0.1.0".into(),
        },
    );
    assert_golden(
        "harness_attach_ack",
        &HarnessAttachAck {
            ok: false,
            message: Some("unknown session".into()),
        },
    );
    assert_golden(
        "forge_request",
        &ForgeRequest {
            session_id: fixed_session_id(),
            broker_token: "tok".into(),
            op: ForgeOp::FetchCredential {
                host: "github.com".into(),
                owner: None,
            },
        },
    );
    assert_golden(
        "upload_request",
        &UploadRequest {
            session_id: fixed_session_id(),
            broker_token: "tok".into(),
            op: UploadOp::ShareFile {
                ext: "png".into(),
                caption: None,
                size_bytes: 4096,
            },
        },
    );
}

/// Writer for the golden corpus. `#[ignore]`d so a normal `cargo test`
/// never regenerates (which would mask a real break). See module header.
#[test]
#[ignore = "regenerates the golden corpus; run explicitly when intentionally evolving a wire type"]
fn regen_golden() {
    use engram_harness_proto::{ForgeRequest, HarnessAttach, HarnessAttachAck, UploadRequest};

    fn write<T: Serialize>(name: &str, value: &T) {
        let bytes = bincode::serialize(value).expect("bincode encode");
        let path = golden_path(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        eprintln!("wrote {} ({} bytes)", path.display(), bytes.len());
    }

    write("frame_event", &HarnessFrame::Event(ev_run_started()));
    write("frame_command", &HarnessFrame::Command(cmd_prompt()));

    write("event_run_started", &ev_run_started());
    write("event_agent_message", &ev_agent_message());
    write("event_tool_call_started", &ev_tool_call_started());
    write("event_tool_call_completed", &ev_tool_call_completed());
    write("event_run_completed", &ev_run_completed());
    write("event_run_interrupted", &ev_run_interrupted());
    write("event_idle", &HarnessEvent::Idle);

    write("agent_role_assistant", &AgentRole::Assistant);
    write("agent_role_user", &AgentRole::User);
    write("agent_role_system", &AgentRole::System);

    write("command_checkpoint", &cmd_checkpoint());
    write("command_shutdown", &cmd_shutdown());
    write("command_prompt", &cmd_prompt());
    write("command_interrupt", &HarnessCommand::Interrupt);

    write("checkpoint_reason_idle", &CheckpointReason::Idle);
    write("checkpoint_reason_preempt", &CheckpointReason::Preempt);
    write("checkpoint_reason_manual", &CheckpointReason::Manual);
    write(
        "checkpoint_reason_run_completed",
        &CheckpointReason::RunCompleted,
    );

    write(
        "forge_op_fetch_credential",
        &ForgeOp::FetchCredential {
            host: "github.com".into(),
            owner: Some("cortexapps".into()),
        },
    );
    write(
        "forge_op_create_pull_request",
        &ForgeOp::CreatePullRequest {
            repo: "cortexapps/engrams".into(),
            head_branch: "feat/x".into(),
            base_branch: "main".into(),
            title: "Add x".into(),
            body: String::new(),
            draft: false,
        },
    );

    write(
        "forge_response_credential",
        &ForgeResponse::Credential {
            username: "x-access-token".into(),
            password: "ghs_x".into(),
        },
    );
    write(
        "forge_response_pull_request",
        &ForgeResponse::PullRequest {
            url: "https://github.com/cortexapps/engrams/pull/1".into(),
            id: 1,
            state: "open".into(),
        },
    );
    write(
        "forge_response_error",
        &ForgeResponse::Error {
            message: "nope".into(),
        },
    );

    write(
        "upload_op_share_file",
        &UploadOp::ShareFile {
            ext: "png".into(),
            caption: Some("the dashboard after my change".into()),
            size_bytes: 4096,
        },
    );

    write(
        "upload_response_shared",
        &UploadResponse::Shared {
            artifact_id: "0190f3a2c0f17e2cba12".into(),
            media_type: "image/png".into(),
            size_bytes: 4096,
        },
    );
    write(
        "upload_response_error",
        &UploadResponse::Error {
            message: "unsupported media type".into(),
        },
    );

    write(
        "harness_attach",
        &HarnessAttach {
            session_id: fixed_session_id(),
            harness_version: "engram-harness-noop/0.1.0".into(),
        },
    );
    write(
        "harness_attach_ack",
        &HarnessAttachAck {
            ok: false,
            message: Some("unknown session".into()),
        },
    );
    write(
        "forge_request",
        &ForgeRequest {
            session_id: fixed_session_id(),
            broker_token: "tok".into(),
            op: ForgeOp::FetchCredential {
                host: "github.com".into(),
                owner: None,
            },
        },
    );
    write(
        "upload_request",
        &UploadRequest {
            session_id: fixed_session_id(),
            broker_token: "tok".into(),
            op: UploadOp::ShareFile {
                ext: "png".into(),
                caption: None,
                size_bytes: 4096,
            },
        },
    );
}
