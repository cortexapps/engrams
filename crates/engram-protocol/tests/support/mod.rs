//! Shared valid-value proptest strategies for the coord ↔ host-agent bincode
//! payloads (`engram_protocol::wire`), consumed by BOTH property suites:
//!   - `decode_never_panics.rs` (ADR 0099 §H4) — mutates these valid frames;
//!   - `codec_roundtrip.rs` (ADR 0099 §H3) — asserts encode→decode identity.
//!
//! Each consumer links a subset, so the module carries a narrow
//! `allow(dead_code)`. Both covered types are structs (no variant enums), so
//! there is no exhaustiveness guard to add — a new struct FIELD is caught by
//! the round-trip property itself (a field the encoder writes but the strategy
//! leaves defaulted still round-trips; the guard is `wire_golden` for a
//! non-trailing insert).
#![allow(dead_code)]

use std::collections::HashMap;

use engram_protocol::wire::{WireExecRequest, WireReapStats};
use proptest::prelude::*;

pub fn s() -> impl Strategy<Value = String> {
    "[ -~]{0,10}"
}

pub fn small_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..=16)
}

pub fn wire_exec_request() -> impl Strategy<Value = WireExecRequest> {
    (
        proptest::collection::vec(s(), 0..3),
        proptest::option::of(small_bytes()),
        proptest::option::of((s(), s())).prop_map(|kv| kv.into_iter().collect::<HashMap<_, _>>()),
        proptest::option::of(s()),
        proptest::option::of(any::<u64>()),
        proptest::option::of(s()),
        proptest::option::of(any::<u64>()),
        proptest::option::of(any::<u64>()),
        proptest::option::of(any::<bool>()),
    )
        .prop_map(
            |(
                command,
                stdin,
                env,
                workdir,
                timeout_ms,
                exec_id,
                stdout_offset,
                stderr_offset,
                wake,
            )| WireExecRequest {
                command,
                stdin,
                env,
                workdir,
                timeout_ms,
                exec_id,
                stdout_offset,
                stderr_offset,
                wake,
            },
        )
}

pub fn wire_reap_stats() -> impl Strategy<Value = WireReapStats> {
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
