//! ADR 0099 H4: decode-never-panics property suite for the host ↔ in-guest
//! agentd vsock wire (`engram_agentd::proto`).
//!
//! Second-highest priority after `engram-harness-proto`: agentd runs inside
//! the sandbox, so the frames the host reads back from it
//! (`WireResponse`, `WireExecEvent`, `AgentReady`) are attacker-
//! influenceable once user code runs in the guest. A host-side decode that
//! panics on a malformed reply is a DoS on the host-agent.
//!
//! Three shapes, per ADR 0099 §H4 — see the harness-proto suite header for
//! the full rationale:
//!   (a) arbitrary byte blobs → every public frame decode never panics;
//!   (b) mutations of valid frames (truncate every prefix / flip / append)
//!       never panic;
//!   (c) allocation bound: `MAX_MSG_BYTES` enforced before the body `vec!`,
//!       and a huge internal length prefix errors rather than over-allocating.

use std::collections::HashMap;
use std::io::Cursor;

use engram_agentd::proto::{
    read_msg, AgentReady, SpawnHarnessRequest, WireDownloadResponse, WireExecEvent,
    WireExecRequest, WireHandshake, WireHandshakeAck, WireRequest, WireResponse, WireStatResponse,
    MAX_MSG_BYTES,
};
use proptest::prelude::*;

// ---- helpers -----------------------------------------------------------

fn decode_every_type(bytes: &[u8]) {
    let _ = bincode::deserialize::<WireRequest>(bytes);
    let _ = bincode::deserialize::<WireResponse>(bytes);
    let _ = bincode::deserialize::<WireExecEvent>(bytes);
    let _ = bincode::deserialize::<WireExecRequest>(bytes);
    let _ = bincode::deserialize::<SpawnHarnessRequest>(bytes);
    let _ = bincode::deserialize::<WireHandshake>(bytes);
    let _ = bincode::deserialize::<WireHandshakeAck>(bytes);
    let _ = bincode::deserialize::<AgentReady>(bytes);
    let _ = bincode::deserialize::<WireStatResponse>(bytes);
    let _ = bincode::deserialize::<WireDownloadResponse>(bytes);
}

fn read_frame_every_type(bytes: &[u8]) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let _ = read_msg::<_, WireRequest>(&mut Cursor::new(bytes)).await;
        let _ = read_msg::<_, WireResponse>(&mut Cursor::new(bytes)).await;
        let _ = read_msg::<_, WireExecEvent>(&mut Cursor::new(bytes)).await;
        let _ = read_msg::<_, AgentReady>(&mut Cursor::new(bytes)).await;
    });
}

// ---- strategies (small values keep the shape-(b) prefix sweep cheap) ---

fn s() -> impl Strategy<Value = String> {
    "[ -~]{0,10}"
}

fn opt_s() -> impl Strategy<Value = Option<String>> {
    proptest::option::of(s())
}

fn small_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..=16)
}

/// ≤1-entry map: matches the golden corpus's determinism note and keeps
/// encodings small.
fn small_env() -> impl Strategy<Value = HashMap<String, String>> {
    proptest::option::of((s(), s())).prop_map(|kv| kv.into_iter().collect())
}

fn wire_exec_event() -> impl Strategy<Value = WireExecEvent> {
    prop_oneof![
        small_bytes().prop_map(WireExecEvent::Stdout),
        small_bytes().prop_map(WireExecEvent::Stderr),
        proptest::option::of(any::<i32>()).prop_map(WireExecEvent::Exit),
    ]
}

fn wire_exec_request() -> impl Strategy<Value = WireExecRequest> {
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

fn spawn_harness() -> impl Strategy<Value = SpawnHarnessRequest> {
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

fn wire_request() -> impl Strategy<Value = WireRequest> {
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

fn wire_response() -> impl Strategy<Value = WireResponse> {
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

fn mutate_and_decode<T: serde::de::DeserializeOwned>(
    encoded: &[u8],
    flip_pos: usize,
    flip_val: u8,
    garbage: &[u8],
) {
    for cut in 0..=encoded.len() {
        let _ = bincode::deserialize::<T>(&encoded[..cut]);
    }
    if !encoded.is_empty() {
        let mut m = encoded.to_vec();
        let p = flip_pos % m.len();
        m[p] ^= flip_val.max(1);
        let _ = bincode::deserialize::<T>(&m);
    }
    let mut ext = encoded.to_vec();
    ext.extend_from_slice(garbage);
    let _ = bincode::deserialize::<T>(&ext);
}

fn weighted_blob() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        10 => proptest::collection::vec(any::<u8>(), 0..=256),
        3 => proptest::collection::vec(any::<u8>(), 257..=4096),
        1 => proptest::collection::vec(any::<u8>(), 4097..=65536),
    ]
}

// ---- shape (a): arbitrary bytes ---------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn arbitrary_bytes_never_panic(bytes in weighted_blob()) {
        decode_every_type(&bytes);
        read_frame_every_type(&bytes);
    }
}

// ---- shape (b): mutations of valid frames ------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mutated_wire_requests_never_panic(
        req in wire_request(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&req).expect("encode");
        mutate_and_decode::<WireRequest>(&encoded, flip_pos, flip_val, &garbage);
    }

    #[test]
    fn mutated_wire_responses_never_panic(
        resp in wire_response(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&resp).expect("encode");
        mutate_and_decode::<WireResponse>(&encoded, flip_pos, flip_val, &garbage);
    }

    #[test]
    fn mutated_exec_events_never_panic(
        ev in wire_exec_event(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&ev).expect("encode");
        mutate_and_decode::<WireExecEvent>(&encoded, flip_pos, flip_val, &garbage);
    }
}

// ---- shape (c): allocation bound ---------------------------------------

#[test]
fn oversized_length_prefix_rejected_before_alloc() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let header = (MAX_MSG_BYTES as u32 + 1).to_be_bytes();
        let err = read_msg::<_, WireResponse>(&mut Cursor::new(header.to_vec()))
            .await
            .expect_err("oversized frame must be rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("MAX_MSG_BYTES"));
    });
}

#[test]
fn huge_internal_length_errors_without_alloc() {
    // WireExecEvent::Stdout(Vec<u8>) — variant index 0, then a u64 Vec
    // length claiming ~18 EiB, with no bytes following. The slice decoder
    // must error on unexpected end, never pre-allocate the claimed length.
    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&u64::MAX.to_le_bytes());
    let err = bincode::deserialize::<WireExecEvent>(&body)
        .expect_err("huge internal length must error, not OOM");
    let _ = err;
}
