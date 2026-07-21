//! ADR 0099 H3 (PR 2): codec ROUND-TRIP identity for the ADR 0045 C2 post-copy
//! page channel (`engram-migrate-proto`).
//!
//! Complement to `decode_never_panics.rs` (H4): those prove garbage never
//! panics on the dest uffd-handler decode path; these prove `encode → decode
//! == identity` for arbitrary valid frames, catching an encode/decode
//! asymmetry on a new field a single golden value would miss. `SealBitmap` is
//! round-tripped as a structural value (any field combination decodes back
//! identically — `validate` legality is a separate, decode-side concern).
//!
//! Valid-value strategies are shared with the decode suite via
//! `tests/support/mod.rs`.

use engram_migrate_proto::{ConnPurpose, FromSource, HandlerControl, SealBitmap, ToSource};
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

roundtrip!(to_source_roundtrips, ToSource, support::to_source());
roundtrip!(from_source_roundtrips, FromSource, support::from_source());
roundtrip!(
    handler_control_roundtrips,
    HandlerControl,
    support::handler_control()
);
roundtrip!(
    conn_purpose_roundtrips,
    ConnPurpose,
    support::conn_purpose()
);
roundtrip!(seal_bitmap_roundtrips, SealBitmap, support::seal_bitmap());
