//! ADR 0099 H3 (PR 2): codec ROUND-TRIP identity for the coord ↔ host-agent
//! bincode payloads (`engram_protocol::wire`).
//!
//! Lowest threat tier (this wire is `WIRE_VERSION`-fenced and trusted — see
//! the decode suite header), included because it is cheap. Complement to
//! `decode_never_panics.rs` (H4) and `wire_golden.rs`: goldens pin one value's
//! bytes, decode-never-panics proves garbage never panics, and these
//! round-trips prove `encode → decode == identity` across the valid-value
//! space — catching an encode/decode asymmetry on a new trailing field.
//!
//! Covers the two crate-local wire payloads; the shared engram-core types the
//! decode suite also fuzzes have no valid-value strategy here (their
//! round-trips belong with their defining crate). Both types gained
//! `PartialEq`/`Eq` for this suite — a no-op on byte layout. Strategies are
//! shared with the decode suite via `tests/support/mod.rs`.

use engram_protocol::wire::{WireExecRequest, WireReapStats};
use proptest::prelude::*;

mod support;

macro_rules! roundtrip {
    ($name:ident, $ty:ty, $strat:expr) => {
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(128))]
            #[test]
            fn $name(value in $strat) {
                let bytes = bincode::serialize(&value).expect("encode");
                let decoded: $ty = bincode::deserialize(&bytes).expect("decode");
                prop_assert_eq!(decoded, value);
            }
        }
    };
}

roundtrip!(
    wire_exec_request_roundtrips,
    WireExecRequest,
    support::wire_exec_request()
);
roundtrip!(
    wire_reap_stats_roundtrips,
    WireReapStats,
    support::wire_reap_stats()
);
