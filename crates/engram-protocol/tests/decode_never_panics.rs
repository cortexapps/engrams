//! ADR 0099 H4: decode-never-panics property suite for the coord ↔ host
//! bincode payloads (`engram_protocol::wire`).
//!
//! **Lowest priority** of the H4 targets by threat model: this wire is
//! version-fenced and trusted — coord and host deploy together and a
//! `WIRE_VERSION` skew is rejected with `failed_precondition` BEFORE any
//! bincode decode (see `wire.rs`), so an attacker never places bytes here.
//! Included because it is cheap: the crate's own payload types decode
//! through the same positional bincode, and a never-panic guard costs
//! almost nothing.
//!
//! These payloads ride inside gRPC `bytes` fields — there is no
//! length-prefix framing layer in this crate, so the decode entry point is
//! `bincode::deserialize` directly. Shapes (a) arbitrary bytes and (b)
//! mutations-of-valid-frames apply; the frame-cap half of (c) lives in the
//! framed crates (harness/agentd/substrate/migrate), so here (c) is just the
//! bincode-slice allocation bound.

use std::collections::HashMap;

use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::SandboxSpec;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_protocol::wire::{WireExecRequest, WireReapStats};
use proptest::prelude::*;

// ---- helpers -----------------------------------------------------------

fn decode_every_type(bytes: &[u8]) {
    // Crate-local payloads.
    let _ = bincode::deserialize::<WireExecRequest>(bytes);
    let _ = bincode::deserialize::<WireReapStats>(bytes);
    // A representative slice of the shared engram-core payloads that cross
    // this boundary inside gRPC `bytes` fields.
    let _ = bincode::deserialize::<ManifestRef>(bytes);
    let _ = bincode::deserialize::<SnapshotMetadata>(bytes);
    let _ = bincode::deserialize::<SandboxSpec>(bytes);
    let _ = bincode::deserialize::<SessionEgressPolicy>(bytes);
}

fn s() -> impl Strategy<Value = String> {
    "[ -~]{0,10}"
}

fn small_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..=16)
}

fn wire_exec_request() -> impl Strategy<Value = WireExecRequest> {
    (
        proptest::collection::vec(s(), 0..3),
        proptest::option::of(small_bytes()),
        proptest::option::of((s(), s())).prop_map(|kv| kv.into_iter().collect::<HashMap<_, _>>()),
        proptest::option::of(s()),
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

fn wire_reap_stats() -> impl Strategy<Value = WireReapStats> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(files_scanned, files_deleted, bytes_freed, unparseable, too_young)| WireReapStats {
                files_scanned,
                files_deleted,
                bytes_freed,
                files_skipped_unparseable: unparseable,
                files_skipped_too_young: too_young,
            },
        )
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
    }
}

// ---- shape (b): mutations of valid frames ------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn mutated_exec_request_never_panic(
        req in wire_exec_request(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&req).expect("encode");
        mutate_and_decode::<WireExecRequest>(&encoded, flip_pos, flip_val, &garbage);
    }

    #[test]
    fn mutated_reap_stats_never_panic(
        stats in wire_reap_stats(),
        flip_pos in any::<usize>(),
        flip_val in any::<u8>(),
        garbage in proptest::collection::vec(any::<u8>(), 0..=32),
    ) {
        let encoded = bincode::serialize(&stats).expect("encode");
        mutate_and_decode::<WireReapStats>(&encoded, flip_pos, flip_val, &garbage);
    }
}

// ---- shape (c): allocation bound (bincode slice) -----------------------

#[test]
fn huge_internal_length_errors_without_alloc() {
    // WireExecRequest.command: Vec<String> is the first field — a u64 outer
    // length claiming ~18 EiB with no elements following. bincode's slice
    // reader (and serde's cautious Vec capacity) bound the work; the decode
    // must error on unexpected end rather than pre-allocating.
    let body = u64::MAX.to_le_bytes().to_vec();
    let err = bincode::deserialize::<WireExecRequest>(&body)
        .expect_err("huge internal length must error, not OOM");
    let _ = err;
}
