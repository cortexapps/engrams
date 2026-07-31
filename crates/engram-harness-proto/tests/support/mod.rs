//! Shared valid-value proptest strategies for the harness ↔ host vsock wire
//! (`engram-harness-proto`), consumed by BOTH property suites:
//!   - `decode_never_panics.rs` (ADR 0099 §H4) — mutates these valid frames;
//!   - `codec_roundtrip.rs` (ADR 0099 §H3) — asserts encode→decode identity.
//!
//! A single source of truth for "what a valid frame looks like" so the two
//! suites never drift. Each consumer links a subset, so the module carries a
//! narrow `allow(dead_code)` — this is a strategy *library*, not dead code.
//!
//! **New wire variant?** The `_exhaustiveness_*` guards below `match` over
//! every enum with NO wildcard arm, so adding a variant is a COMPILE error
//! here until you add a generator arm to the matching strategy. Do not add a
//! `_ =>` arm to silence it — that reintroduces the silent-gap class this
//! guards against (the BrowserActivity gap that motivated ADR 0099 H3 PR 2).
#![allow(dead_code)]

use engram_core::{SandboxId, SessionId};
use engram_harness_proto::{
    AgentRole, AttachReject, CheckpointAck, CheckpointReason, EditHunk, FileChange, ForgeOp,
    ForgeRequest, ForgeResponse, HarnessAttach, HarnessAttachAck, HarnessCommand, HarnessEvent,
    HarnessFrame, RelayAck, RelayConnect, UploadOp, UploadRequest, UploadResponse,
};
use proptest::prelude::*;

// ---- primitive strategies (small values keep the shape-(b) truncation
//      sweep in the decode suite cheap) -----------------------------------

/// Small printable-ASCII string.
pub fn s() -> impl Strategy<Value = String> {
    "[ -~]{0,10}"
}

pub fn opt_s() -> impl Strategy<Value = Option<String>> {
    proptest::option::of(s())
}

pub fn session_id() -> impl Strategy<Value = SessionId> {
    any::<[u8; 16]>().prop_map(|b| SessionId(uuid::Uuid::from_bytes(b)))
}

pub fn sandbox_id() -> impl Strategy<Value = SandboxId> {
    any::<[u8; 16]>().prop_map(|b| SandboxId(uuid::Uuid::from_bytes(b)))
}

// ---- component enums / structs -----------------------------------------

pub fn agent_role() -> impl Strategy<Value = AgentRole> {
    prop_oneof![
        Just(AgentRole::Assistant),
        Just(AgentRole::User),
        Just(AgentRole::System),
    ]
}

pub fn file_change() -> impl Strategy<Value = FileChange> {
    prop_oneof![
        s().prop_map(|content| FileChange::Write { content }),
        proptest::collection::vec(
            (s(), s()).prop_map(|(old, new)| EditHunk { old, new }),
            0..3
        )
        .prop_map(|hunks| FileChange::Edit { hunks }),
        s().prop_map(|unified_diff| FileChange::Patch { unified_diff }),
    ]
}

pub fn harness_event() -> impl Strategy<Value = HarnessEvent> {
    prop_oneof![
        (s(), opt_s(), opt_s()).prop_map(|(run_id, prompt_summary, prompt_id)| {
            HarnessEvent::RunStarted {
                run_id,
                prompt_summary,
                prompt_id,
            }
        }),
        (s(), s(), agent_role(), s()).prop_map(|(run_id, message_id, role, text)| {
            HarnessEvent::AgentMessage {
                run_id,
                message_id,
                role,
                text,
            }
        }),
        (s(), s(), s(), opt_s()).prop_map(|(run_id, tool_call_id, tool_name, args_summary)| {
            HarnessEvent::ToolCallStarted {
                run_id,
                tool_call_id,
                tool_name,
                args_summary,
            }
        }),
        (s(), s(), s(), any::<bool>(), any::<u64>(), opt_s()).prop_map(
            |(run_id, tool_call_id, tool_name, ok, duration_ms, result_summary)| {
                HarnessEvent::ToolCallCompleted {
                    run_id,
                    tool_call_id,
                    tool_name,
                    ok,
                    duration_ms,
                    result_summary,
                }
            }
        ),
        (s(), any::<bool>()).prop_map(|(run_id, ok)| HarnessEvent::RunCompleted { run_id, ok }),
        s().prop_map(|run_id| HarnessEvent::RunInterrupted { run_id }),
        Just(HarnessEvent::Idle),
        (s(), opt_s())
            .prop_map(|(prompt_id, summary)| HarnessEvent::PromptQueued { prompt_id, summary }),
        (s(), opt_s())
            .prop_map(|(prompt_id, summary)| HarnessEvent::PromptEdited { prompt_id, summary }),
        s().prop_map(|prompt_id| HarnessEvent::PromptDequeued { prompt_id }),
        (s(), s(), s()).prop_map(
            |(run_id, message_id, chunk)| HarnessEvent::AgentMessageChunk {
                run_id,
                message_id,
                chunk
            }
        ),
        (s(), s(), s(), file_change()).prop_map(|(run_id, tool_call_id, path, change)| {
            HarnessEvent::FileChanged {
                run_id,
                tool_call_id,
                path,
                change,
            }
        }),
        s().prop_map(|title| HarnessEvent::TitleSuggested { title }),
        s().prop_map(|prompt_id| HarnessEvent::PromptSteered { prompt_id }),
        (s(), s(), s(), s()).prop_map(|(run_id, call_id, name, args_json)| {
            HarnessEvent::ToolCallRequested {
                run_id,
                call_id,
                name,
                args_json,
            }
        }),
        Just(HarnessEvent::Parked),
        // PR #693: browser-driving shell tool call. Added here (and thereby to
        // both the decode-mutation corpus and the round-trip identity suite)
        // by ADR 0099 H3 PR 2 — the variant that motivated the guard below.
        (s(), s(), s()).prop_map(|(run_id, tool_call_id, intent)| {
            HarnessEvent::BrowserActivity {
                run_id,
                tool_call_id,
                intent,
            }
        }),
    ]
}

pub fn checkpoint_reason() -> impl Strategy<Value = CheckpointReason> {
    prop_oneof![
        Just(CheckpointReason::Idle),
        Just(CheckpointReason::Preempt),
        Just(CheckpointReason::Manual),
        Just(CheckpointReason::RunCompleted),
    ]
}

pub fn harness_command() -> impl Strategy<Value = HarnessCommand> {
    prop_oneof![
        checkpoint_reason().prop_map(|reason| HarnessCommand::Checkpoint { reason }),
        any::<u32>().prop_map(|grace_secs| HarnessCommand::Shutdown { grace_secs }),
        (s(), s()).prop_map(|(text, prompt_id)| HarnessCommand::Prompt { text, prompt_id }),
        Just(HarnessCommand::Interrupt),
        (s(), s()).prop_map(|(prompt_id, text)| HarnessCommand::EditQueued { prompt_id, text }),
        s().prop_map(|prompt_id| HarnessCommand::DequeueQueued { prompt_id }),
        (s(), s()).prop_map(|(call_id, result_json)| HarnessCommand::ToolResult {
            call_id,
            result_json
        }),
    ]
}

pub fn harness_frame() -> impl Strategy<Value = HarnessFrame> {
    prop_oneof![
        harness_event().prop_map(HarnessFrame::Event),
        harness_command().prop_map(HarnessFrame::Command),
    ]
}

pub fn harness_attach() -> impl Strategy<Value = HarnessAttach> {
    (session_id(), sandbox_id(), any::<u64>(), s()).prop_map(
        |(session_id, sandbox_id, binding_epoch, harness_version)| HarnessAttach {
            session_id,
            sandbox_id,
            binding_epoch,
            harness_version,
        },
    )
}

pub fn attach_reject() -> impl Strategy<Value = AttachReject> {
    prop_oneof![
        Just(AttachReject::UnknownBinding),
        Just(AttachReject::Superseded),
        Just(AttachReject::SessionMismatch),
    ]
}

pub fn harness_attach_ack() -> impl Strategy<Value = HarnessAttachAck> {
    (
        any::<bool>(),
        proptest::option::of(attach_reject()),
        opt_s(),
    )
        .prop_map(|(ok, reject, message)| HarnessAttachAck {
            ok,
            reject,
            message,
        })
}

pub fn checkpoint_ack() -> impl Strategy<Value = CheckpointAck> {
    (any::<bool>(), opt_s()).prop_map(|(ok, message)| CheckpointAck { ok, message })
}

pub fn forge_request() -> impl Strategy<Value = ForgeRequest> {
    (session_id(), s(), forge_op()).prop_map(|(session_id, broker_token, op)| ForgeRequest {
        session_id,
        broker_token,
        op,
    })
}

fn forge_op() -> impl Strategy<Value = ForgeOp> {
    prop_oneof![
        (s(), opt_s()).prop_map(|(host, owner)| ForgeOp::FetchCredential { host, owner }),
        Just(ForgeOp::FetchOAuthCredential),
        (any::<i64>(), proptest::collection::vec(any::<u8>(), 0..64)).prop_map(
            |(expected_version, opaque_bundle)| ForgeOp::UpdateOAuthCredential {
                expected_version,
                opaque_bundle,
            }
        ),
    ]
}

pub fn forge_response() -> impl Strategy<Value = ForgeResponse> {
    prop_oneof![
        (s(), s())
            .prop_map(|(username, password)| ForgeResponse::Credential { username, password }),
        (
            s(),
            any::<i64>(),
            proptest::collection::vec(any::<u8>(), 0..64)
        )
            .prop_map(|(provider, version, opaque_bundle)| {
                ForgeResponse::OAuthCredential {
                    provider,
                    version,
                    opaque_bundle,
                }
            }),
        s().prop_map(|message| ForgeResponse::Error { message }),
    ]
}

pub fn upload_request() -> impl Strategy<Value = UploadRequest> {
    (session_id(), s(), s(), opt_s(), any::<u64>()).prop_map(
        |(session_id, broker_token, ext, caption, size_bytes)| UploadRequest {
            session_id,
            broker_token,
            op: UploadOp::ShareFile {
                ext,
                caption,
                size_bytes,
            },
        },
    )
}

pub fn upload_response() -> impl Strategy<Value = UploadResponse> {
    prop_oneof![
        (s(), s(), any::<u64>()).prop_map(|(artifact_id, media_type, size_bytes)| {
            UploadResponse::Shared {
                artifact_id,
                media_type,
                size_bytes,
            }
        }),
        s().prop_map(|message| UploadResponse::Error { message }),
    ]
}

pub fn relay_connect() -> impl Strategy<Value = RelayConnect> {
    any::<u16>().prop_map(|target_port| RelayConnect { target_port })
}

pub fn relay_ack() -> impl Strategy<Value = RelayAck> {
    (any::<bool>(), opt_s()).prop_map(|(ok, error)| RelayAck { ok, error })
}

// ---- exhaustiveness guards ---------------------------------------------
//
// Never called; compiled only so the `match` is checked. A new enum variant
// makes the match non-exhaustive → compile error → you land here and add the
// corresponding generator arm above. NO wildcard arms.

fn _exhaustiveness_harness_event(e: &HarnessEvent) {
    match e {
        HarnessEvent::RunStarted { .. } => {}
        HarnessEvent::AgentMessage { .. } => {}
        HarnessEvent::ToolCallStarted { .. } => {}
        HarnessEvent::ToolCallCompleted { .. } => {}
        HarnessEvent::RunCompleted { .. } => {}
        HarnessEvent::RunInterrupted { .. } => {}
        HarnessEvent::Idle => {}
        HarnessEvent::PromptQueued { .. } => {}
        HarnessEvent::PromptEdited { .. } => {}
        HarnessEvent::PromptDequeued { .. } => {}
        HarnessEvent::AgentMessageChunk { .. } => {}
        HarnessEvent::FileChanged { .. } => {}
        HarnessEvent::TitleSuggested { .. } => {}
        HarnessEvent::PromptSteered { .. } => {}
        HarnessEvent::ToolCallRequested { .. } => {}
        HarnessEvent::Parked => {}
        HarnessEvent::BrowserActivity { .. } => {}
    }
}

fn _exhaustiveness_harness_command(c: &HarnessCommand) {
    match c {
        HarnessCommand::Checkpoint { .. } => {}
        HarnessCommand::Shutdown { .. } => {}
        HarnessCommand::Prompt { .. } => {}
        HarnessCommand::Interrupt => {}
        HarnessCommand::EditQueued { .. } => {}
        HarnessCommand::DequeueQueued { .. } => {}
        HarnessCommand::ToolResult { .. } => {}
    }
}

fn _exhaustiveness_harness_frame(f: &HarnessFrame) {
    match f {
        HarnessFrame::Event(_) => {}
        HarnessFrame::Command(_) => {}
    }
}

fn _exhaustiveness_file_change(c: &FileChange) {
    match c {
        FileChange::Write { .. } => {}
        FileChange::Edit { .. } => {}
        FileChange::Patch { .. } => {}
    }
}

fn _exhaustiveness_agent_role(r: &AgentRole) {
    match r {
        AgentRole::Assistant => {}
        AgentRole::User => {}
        AgentRole::System => {}
    }
}

fn _exhaustiveness_checkpoint_reason(r: &CheckpointReason) {
    match r {
        CheckpointReason::Idle => {}
        CheckpointReason::Preempt => {}
        CheckpointReason::Manual => {}
        CheckpointReason::RunCompleted => {}
    }
}

fn _exhaustiveness_attach_reject(r: &AttachReject) {
    match r {
        AttachReject::UnknownBinding => {}
        AttachReject::Superseded => {}
        AttachReject::SessionMismatch => {}
    }
}

fn _exhaustiveness_forge_op(o: &ForgeOp) {
    match o {
        ForgeOp::FetchCredential { .. } => {}
        ForgeOp::FetchOAuthCredential => {}
        ForgeOp::UpdateOAuthCredential { .. } => {}
    }
}

fn _exhaustiveness_forge_response(r: &ForgeResponse) {
    match r {
        ForgeResponse::Credential { .. } => {}
        ForgeResponse::OAuthCredential { .. } => {}
        ForgeResponse::Error { .. } => {}
    }
}

fn _exhaustiveness_upload_op(o: &UploadOp) {
    match o {
        UploadOp::ShareFile { .. } => {}
    }
}

fn _exhaustiveness_upload_response(r: &UploadResponse) {
    match r {
        UploadResponse::Shared { .. } => {}
        UploadResponse::Error { .. } => {}
    }
}
