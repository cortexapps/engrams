//! Arbitrary and mutated bytes must not panic in host payload decoders.
//! Ordinary policy decoding also rejects incomplete and trailing bytes.

use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::SandboxSpec;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_protocol::wire::WireExecRequest;
use proptest::prelude::*;

// The valid-value strategies live in the shared `support` module; ADR 0099
// H3's `codec_roundtrip.rs` reuses the exact same generators.
mod support;
use support::wire_exec_request;

// ---- helpers -----------------------------------------------------------

fn decode_every_type(bytes: &[u8]) {
    use prost::Message;
    let _ = engram_protocol::mcp::ReserveManagedCreateRequest::decode(bytes);
    let _ = engram_protocol::mcp::ReadNativeManifestRequest::decode(bytes);
    let _ = engram_protocol::mcp::ReadNativeManifestResponse::decode(bytes);

    let _ = engram_protocol::managed_policy::decode::<
        engram_core::types::managed_deny::ManagedPolicyApply,
    >(bytes);
    let _ = engram_protocol::managed_policy::decode::<
        Option<engram_core::types::managed_deny::ManagedAppliedState>,
    >(bytes);
    let _ = engram_protocol::ordinary_policy::decode(bytes);
    // Crate-local payloads.
    let _ = bincode::deserialize::<WireExecRequest>(bytes);
    // A representative slice of the shared engram-core payloads that cross
    // this boundary inside gRPC `bytes` fields.
    let _ = bincode::deserialize::<ManifestRef>(bytes);
    let _ = bincode::deserialize::<SnapshotMetadata>(bytes);
    let _ = bincode::deserialize::<SandboxSpec>(bytes);
    let _ = bincode::deserialize::<SessionEgressPolicy>(bytes);
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
    #![proptest_config(ProptestConfig {
        cases: 128,
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct(
            concat!(env!("CARGO_MANIFEST_DIR"), "/proptest-regressions/decode_never_panics.txt")
        ))),
        ..ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_bytes_never_panic(bytes in weighted_blob()) {
        decode_every_type(&bytes);
    }
}

// ---- shape (b): mutations of valid frames ------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct(
            concat!(env!("CARGO_MANIFEST_DIR"), "/proptest-regressions/decode_never_panics.txt")
        ))),
        ..ProptestConfig::default()
    })]

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
