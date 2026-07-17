//! Shared valid-value proptest strategies for the host ↔ in-guest agentd
//! vsock wire (`engram_agentd::proto`), consumed by BOTH property suites:
//!   - `decode_never_panics.rs` (ADR 0099 §H4) — mutates these valid frames;
//!   - `codec_roundtrip.rs` (ADR 0099 §H3) — asserts encode→decode identity.
//!
//! Each consumer links a subset, so the module carries a narrow
//! `allow(dead_code)` — this is a strategy *library*, not dead code.
//!
//! **New wire variant?** The `_exhaustiveness_*` guards below `match` over
//! every enum with NO wildcard arm, so adding a variant is a COMPILE error
//! here until you add a generator arm to the matching strategy. Both
//! `WireRequest`/`WireResponse` are documented APPEND-ONLY — the guard makes
//! "appended but not generated" fail the build rather than silently skip.
#![allow(dead_code)]

use std::collections::HashMap;

use engram_agentd::proto::{
    AgentReady, SpawnHarnessRequest, WireDownloadResponse, WireExecEvent, WireExecRequest,
    WireHandshake, WireHandshakeAck, WireRequest, WireResponse, WireStatResponse,
};
use proptest::prelude::*;

// ---- primitives --------------------------------------------------------

pub fn s() -> impl Strategy<Value = String> {
    "[ -~]{0,10}"
}

pub fn opt_s() -> impl Strategy<Value = Option<String>> {
    proptest::option::of(s())
}

pub fn small_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..=16)
}

/// ≤1-entry map: matches the golden corpus's determinism note and keeps
/// encodings small.
pub fn small_env() -> impl Strategy<Value = HashMap<String, String>> {
    proptest::option::of((s(), s())).prop_map(|kv| kv.into_iter().collect())
}

// ---- frames ------------------------------------------------------------

pub fn wire_exec_event() -> impl Strategy<Value = WireExecEvent> {
    prop_oneof![
        small_bytes().prop_map(WireExecEvent::Stdout),
        small_bytes().prop_map(WireExecEvent::Stderr),
        proptest::option::of(any::<i32>()).prop_map(WireExecEvent::Exit),
    ]
}

pub fn wire_exec_request() -> impl Strategy<Value = WireExecRequest> {
    (
        proptest::collection::vec(s(), 0..3),
        proptest::option::of(small_bytes()),
        small_env(),
        opt_s(),
        proptest::option::of(any::<u64>()),
    )
        .prop_map(
            |(command, stdin, env, workdir, timeout_ms)| WireExecRequest {
                command,
                stdin,
                env,
                workdir,
                timeout_ms,
            },
        )
}

pub fn spawn_harness() -> impl Strategy<Value = SpawnHarnessRequest> {
    (
        proptest::collection::vec(s(), 0..3),
        small_env(),
        small_env(),
        opt_s(),
    )
        .prop_map(
            |(argv, env, session_env, host_ca_pem)| SpawnHarnessRequest {
                argv,
                env,
                session_env,
                host_ca_pem,
            },
        )
}

pub fn wire_request() -> impl Strategy<Value = WireRequest> {
    prop_oneof![
        wire_exec_request().prop_map(WireRequest::Exec),
        s().prop_map(|path| WireRequest::Stat { path }),
        (s(), small_bytes(), proptest::option::of(any::<u32>()))
            .prop_map(|(path, bytes, mode)| WireRequest::Upload { path, bytes, mode }),
        s().prop_map(|path| WireRequest::Download { path }),
        Just(WireRequest::Ping),
        Just(WireRequest::Shutdown),
        Just(WireRequest::GuestIp),
        proptest::option::of(any::<u16>()).prop_map(|port| WireRequest::StartShell { port }),
        spawn_harness().prop_map(WireRequest::SpawnHarness),
        Just(WireRequest::Sync),
        proptest::option::of(any::<u16>()).prop_map(|port| WireRequest::StartBrowser { port }),
        Just(WireRequest::StopBrowser),
        Just(WireRequest::RefreshAgent),
        proptest::option::of(any::<u16>()).prop_map(|port| WireRequest::StartIde { port }),
        Just(WireRequest::StopIde),
        any::<i64>().prop_map(|unix_nanos| WireRequest::StepClock { unix_nanos }),
    ]
}

pub fn wire_response() -> impl Strategy<Value = WireResponse> {
    prop_oneof![
        (any::<bool>(), any::<u64>(), any::<i64>(), any::<bool>()).prop_map(
            |(exists, size, mtime_unix, is_dir)| WireResponse::Stat(WireStatResponse {
                exists,
                size,
                mtime_unix,
                is_dir,
            })
        ),
        Just(WireResponse::UploadOk),
        small_bytes().prop_map(|bytes| WireResponse::Download(WireDownloadResponse { bytes })),
        Just(WireResponse::Pong),
        Just(WireResponse::ShutdownAck),
        opt_s().prop_map(WireResponse::GuestIp),
        (any::<u16>(), any::<bool>())
            .prop_map(|(port, spawned)| WireResponse::ShellReady { port, spawned }),
        (
            proptest::option::of(any::<u32>()),
            proptest::option::of(any::<bool>())
        )
            .prop_map(|(pid, ca_changed)| WireResponse::HarnessSpawned { pid, ca_changed }),
        (s(), s()).prop_map(|(kind, message)| WireResponse::Error { kind, message }),
        Just(WireResponse::Synced),
        (any::<u16>(), any::<bool>(), opt_s()).prop_map(|(port, spawned, cdp_warning)| {
            WireResponse::BrowserReady {
                port,
                spawned,
                cdp_warning,
            }
        }),
        Just(WireResponse::BrowserStopped),
        (any::<bool>(), opt_s())
            .prop_map(|(restarting, sha256)| WireResponse::AgentRefreshed { restarting, sha256 }),
        (any::<u16>(), any::<bool>())
            .prop_map(|(port, spawned)| WireResponse::IdeReady { port, spawned }),
        Just(WireResponse::IdeStopped),
        proptest::option::of(any::<i64>()).prop_map(|applied_offset_nanos| {
            WireResponse::ClockStepped {
                applied_offset_nanos,
            }
        }),
    ]
}

pub fn wire_handshake() -> impl Strategy<Value = WireHandshake> {
    (s(), s()).prop_map(|(token, agent_version)| WireHandshake {
        token,
        agent_version,
    })
}

pub fn wire_handshake_ack() -> impl Strategy<Value = WireHandshakeAck> {
    (any::<bool>(), opt_s()).prop_map(|(ok, message)| WireHandshakeAck { ok, message })
}

pub fn agent_ready() -> impl Strategy<Value = AgentReady> {
    s().prop_map(|agent_version| AgentReady { agent_version })
}

pub fn wire_stat_response() -> impl Strategy<Value = WireStatResponse> {
    (any::<bool>(), any::<u64>(), any::<i64>(), any::<bool>()).prop_map(
        |(exists, size, mtime_unix, is_dir)| WireStatResponse {
            exists,
            size,
            mtime_unix,
            is_dir,
        },
    )
}

pub fn wire_download_response() -> impl Strategy<Value = WireDownloadResponse> {
    small_bytes().prop_map(|bytes| WireDownloadResponse { bytes })
}

// ---- exhaustiveness guards ---------------------------------------------
//
// Never called; compiled only so the `match` is checked. A new enum variant
// makes the match non-exhaustive → compile error → add the matching generator
// arm above. NO wildcard arms.

fn _exhaustiveness_wire_request(r: &WireRequest) {
    match r {
        WireRequest::Exec(_) => {}
        WireRequest::Stat { .. } => {}
        WireRequest::Upload { .. } => {}
        WireRequest::Download { .. } => {}
        WireRequest::Ping => {}
        WireRequest::Shutdown => {}
        WireRequest::GuestIp => {}
        WireRequest::StartShell { .. } => {}
        WireRequest::SpawnHarness(_) => {}
        WireRequest::Sync => {}
        WireRequest::StartBrowser { .. } => {}
        WireRequest::StopBrowser => {}
        WireRequest::RefreshAgent => {}
        WireRequest::StartIde { .. } => {}
        WireRequest::StopIde => {}
        WireRequest::StepClock { .. } => {}
    }
}

fn _exhaustiveness_wire_response(r: &WireResponse) {
    match r {
        WireResponse::Stat(_) => {}
        WireResponse::UploadOk => {}
        WireResponse::Download(_) => {}
        WireResponse::Pong => {}
        WireResponse::ShutdownAck => {}
        WireResponse::GuestIp(_) => {}
        WireResponse::ShellReady { .. } => {}
        WireResponse::HarnessSpawned { .. } => {}
        WireResponse::Error { .. } => {}
        WireResponse::Synced => {}
        WireResponse::BrowserReady { .. } => {}
        WireResponse::BrowserStopped => {}
        WireResponse::AgentRefreshed { .. } => {}
        WireResponse::IdeReady { .. } => {}
        WireResponse::IdeStopped => {}
        WireResponse::ClockStepped { .. } => {}
    }
}

fn _exhaustiveness_wire_exec_event(e: &WireExecEvent) {
    match e {
        WireExecEvent::Stdout(_) => {}
        WireExecEvent::Stderr(_) => {}
        WireExecEvent::Exit(_) => {}
    }
}
