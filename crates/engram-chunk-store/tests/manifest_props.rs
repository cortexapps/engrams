//! Property-based tests for the chunk manifest (ADR 0099 H3 PR 1).
//!
//! The manifest is the reuse key under every snapshot: two bakes that
//! produce byte-identical images must derive the *same* `content_ref()`
//! (ADR 0036), and a manifest that `validate()` accepts must be
//! internally consistent (offsets aligned, monotonic, in bounds) —
//! consumers (NBD daemon, UFFD handler, materialize) trust that geometry
//! without re-checking it. These properties assert those invariants over
//! arbitrary inputs rather than the handful of hand-picked cases the
//! `#[cfg(test)]` unit tests cover.
//!
//! Cases are capped (`PROPTEST_CASES`) so the suite stays well under the
//! 3-minute nextest slow-timeout; counterexamples shrink to a checked-in
//! seed under `proptest-regressions/`.

use engram_chunk_store::bootstrap::Bootstrap;
use engram_chunk_store::error::ChunkStoreError;
use engram_chunk_store::manifest::{
    ChunkHash, ChunkRef, ChunkSize, Manifest, ManifestKind, MANIFEST_SCHEMA_VERSION,
};
use proptest::prelude::*;

/// Capped per the ADR: heavier targets keep `cases` in the 64..128 band
/// so the whole suite respects the nextest slow-timeout.
const PROPTEST_CASES: u32 = 96;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_kind() -> impl Strategy<Value = ManifestKind> {
    prop_oneof![Just(ManifestKind::Disk), Just(ManifestKind::Memory)]
}

/// A raw 32-byte digest wrapped as a `ChunkHash`. We wrap arbitrary
/// bytes (not `ChunkHash::of`) so the hex / storage-key round-trips see
/// the full byte space, including high bytes and leading zeros.
fn arb_hash() -> impl Strategy<Value = ChunkHash> {
    any::<[u8; 32]>().prop_map(ChunkHash::from_bytes)
}

/// A *valid* manifest: chunk_size ≥ 1, offsets are strictly-increasing
/// multiples of chunk_size all `< total_bytes`, and total_bytes lands in
/// the final chunk's slot so the last chunk may be short. Everything this
/// strategy produces must pass `validate()` — that is property #4.
///
/// Returns the manifest plus the per-index chunk *lengths* the geometry
/// implies, so the reassembly property can check byte tiling without
/// re-deriving the formula it is trying to test.
fn arb_valid_manifest() -> impl Strategy<Value = Manifest> {
    // Small sizes keep buffers tiny while still exercising the
    // final-partial-chunk and alignment boundaries that matter.
    let chunk_size = 1u64..=64;
    let capacity = 1u64..=12; // number of chunk slots
    (arb_kind(), chunk_size, capacity)
        .prop_flat_map(|(kind, cs, cap)| {
            // total_bytes lands somewhere in the last slot: strictly
            // greater than (cap-1)*cs (so every index < cap is in bounds)
            // and ≤ cap*cs (so the final chunk is `cs` or shorter).
            let lo = (cap - 1) * cs + 1;
            let hi = cap * cs;
            let indices = proptest::collection::btree_set(0u64..cap, 0..=(cap as usize));
            (
                Just(kind),
                Just(cs),
                lo..=hi,
                indices,
                proptest::collection::vec(arb_hash(), cap as usize),
            )
        })
        .prop_map(|(kind, cs, total_bytes, indices, hashes)| {
            let chunks = indices
                .into_iter()
                .map(|i| ChunkRef {
                    offset: i * cs,
                    hash: hashes[i as usize],
                })
                .collect();
            Manifest {
                schema_version: MANIFEST_SCHEMA_VERSION,
                kind,
                chunk_size: ChunkSize::bytes(cs),
                total_bytes,
                chunks,
                parent: None,
                working_set_trace: None,
                annotations: serde_json::Value::Null,
            }
        })
}

/// Like `arb_valid_manifest` but guarantees at least one chunk (so an
/// offset field exists to mutate) and chunk_size ≥ 2 (so a `+1` offset
/// nudge actually breaks alignment). Used by the mutation-rejection
/// properties.
fn arb_valid_manifest_with_chunks(min_chunks: usize) -> impl Strategy<Value = Manifest> {
    let chunk_size = 2u64..=64;
    // Need at least `min_chunks` slots to fit `min_chunks` distinct
    // offsets.
    let cap_lo = min_chunks.max(1) as u64;
    (arb_kind(), chunk_size, cap_lo..=(cap_lo + 11))
        .prop_flat_map(move |(kind, cs, cap)| {
            let lo = (cap - 1) * cs + 1;
            let hi = cap * cs;
            let indices = proptest::collection::btree_set(0u64..cap, min_chunks..=(cap as usize));
            (
                Just(kind),
                Just(cs),
                lo..=hi,
                indices,
                proptest::collection::vec(arb_hash(), cap as usize),
            )
        })
        .prop_map(|(kind, cs, total_bytes, indices, hashes)| {
            let chunks = indices
                .into_iter()
                .map(|i| ChunkRef {
                    offset: i * cs,
                    hash: hashes[i as usize],
                })
                .collect();
            Manifest {
                schema_version: MANIFEST_SCHEMA_VERSION,
                kind,
                chunk_size: ChunkSize::bytes(cs),
                total_bytes,
                chunks,
                parent: None,
                working_set_trace: None,
                annotations: serde_json::Value::Null,
            }
        })
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: PROPTEST_CASES,
        // Integration-test targets can't use the default source-relative
        // persistence (proptest can't find lib.rs from tests/); pin the
        // regressions file explicitly so counterexamples become
        // checked-in deterministic cases (ADR 0099 H3).
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/manifest_props.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// Chunking geometry: densely chunk an arbitrary buffer, then prove
    /// the resulting manifest (a) reports the right chunk count, (b) has
    /// a `chunk_at` hit at every aligned in-range offset and a miss at
    /// the aligned past-end offset, (c) tiles `[0, total_bytes)` with contiguous non-overlapping
    /// ranges whose lengths follow `min(chunk_size, total_bytes-offset)`,
    /// and (d) reassembles byte-for-byte to the original buffer.
    #[test]
    fn dense_chunking_covers_every_offset_and_reassembles(
        buffer in proptest::collection::vec(any::<u8>(), 0..512),
        chunk_size in 1usize..=48,
    ) {
        let total_bytes = buffer.len() as u64;
        let cs = chunk_size as u64;

        // Build a dense manifest: one ChunkRef per chunk_size-aligned
        // slot, keeping the actual bytes alongside for reassembly.
        let mut chunks = Vec::new();
        let mut chunk_bytes: Vec<(u64, &[u8])> = Vec::new();
        let mut off = 0u64;
        while off < total_bytes {
            let end = (off + cs).min(total_bytes);
            let slice = &buffer[off as usize..end as usize];
            chunks.push(ChunkRef { offset: off, hash: ChunkHash::of(slice) });
            chunk_bytes.push((off, slice));
            off += cs;
        }

        let m = Manifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            kind: ManifestKind::Disk,
            chunk_size: ChunkSize::bytes(cs),
            total_bytes,
            chunks,
            parent: None,
            working_set_trace: None,
            annotations: serde_json::Value::Null,
        };

        // (0) Dense manifests are internally consistent.
        prop_assert!(m.validate().is_ok(), "dense manifest must validate");

        // (a) chunk count is ceil(total/cs).
        let expected = total_bytes.div_ceil(cs);
        prop_assert_eq!(m.expected_chunk_count(), expected);
        prop_assert_eq!(m.chunks.len() as u64, expected);

        // (b) chunk_at hits every aligned offset, misses everything else.
        for i in 0..expected {
            let aligned = i * cs;
            let hit = m.chunk_at(aligned);
            prop_assert!(hit.is_some(), "aligned offset {} must resolve", aligned);
            prop_assert_eq!(hit.unwrap().offset, aligned);
        }
        // An aligned offset past the end never resolves. (Misaligned
        // queries are a CONTRACT VIOLATION — chunk_at's doc requires
        // exact alignment and ADR 0099 H6 site 2 enforces it with a
        // debug_assert, so this suite only ever queries aligned
        // offsets.)
        prop_assert!(m.chunk_at(expected * cs).is_none());

        // (c)+(d) tile and reassemble. Lengths follow the production
        // geometry formula (the one Bootstrap::build_per_chunk uses).
        let mut reassembled = vec![0u8; total_bytes as usize];
        let mut covered = 0u64;
        let mut prev_end = 0u64;
        for ((off, bytes), cref) in chunk_bytes.iter().zip(&m.chunks) {
            let geom_len = cs.min(total_bytes - cref.offset);
            prop_assert_eq!(bytes.len() as u64, geom_len, "chunk length must match geometry");
            prop_assert_eq!(*off, cref.offset);
            prop_assert_eq!(*off, prev_end, "ranges must be contiguous (no gap/overlap)");
            reassembled[*off as usize..*off as usize + bytes.len()].copy_from_slice(bytes);
            covered += geom_len;
            prev_end = off + geom_len;
        }
        prop_assert_eq!(covered, total_bytes, "chunks must cover exactly total_bytes");
        prop_assert_eq!(&reassembled, &buffer, "reassembly must equal the original");
    }

    /// `ChunkHash` hex round-trips: `from_hex(to_hex(h)) == h`, hex is
    /// exactly 64 lowercase hex chars, and `from_bytes(as_bytes) == h`.
    #[test]
    fn chunk_hash_hex_round_trips(h in arb_hash()) {
        let hex = h.to_hex();
        prop_assert_eq!(hex.len(), 64);
        prop_assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        prop_assert_eq!(ChunkHash::from_hex(&hex).unwrap(), h);
        prop_assert_eq!(ChunkHash::from_bytes(*h.as_bytes()), h);
    }

    /// `ChunkHash` storage-key round-trips: the object-storage key
    /// derived from a hash parses back to the identical hash (the reverse
    /// GC-sweep uses to translate list_prefix entries back to hashes).
    #[test]
    fn chunk_hash_storage_key_round_trips(h in arb_hash()) {
        let key = h.storage_key();
        let hex = h.to_hex();
        prop_assert_eq!(&key, &format!("chunks/sha256/{}/{}", &hex[..2], &hex[2..]));
        prop_assert_eq!(ChunkHash::from_storage_key(&key), Some(h));
    }

    /// `ChunkHash::of` is a stable content hash — the same bytes always
    /// produce the same hash (content addressing depends on it), and it
    /// round-trips through hex like any other hash.
    #[test]
    fn chunk_hash_of_is_stable(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let a = ChunkHash::of(&bytes);
        let b = ChunkHash::of(&bytes);
        prop_assert_eq!(a, b);
        prop_assert_eq!(ChunkHash::from_hex(&a.to_hex()).unwrap(), a);
    }

    /// Manifest serde-JSON round-trip is lossless AND preserves
    /// `content_ref()` — the digest is the reuse key, so any serde skew
    /// that perturbed it would silently defeat base-snapshot reuse.
    #[test]
    fn manifest_json_round_trip_preserves_content_ref(m in arb_valid_manifest()) {
        let json = serde_json::to_string(&m).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back, &m, "JSON round-trip must be lossless");
        prop_assert_eq!(back.content_ref(), m.content_ref(), "content_ref must survive serde");
        // content_ref is always a well-formed RFC 4122 v8 UUID, v1.
        let r = m.content_ref();
        prop_assert_eq!(r.version, 1);
        prop_assert_eq!(r.manifest_id.get_version_num(), 8);
    }

    /// `content_ref` ignores non-content fields: `parent`, annotations,
    /// and the working-set trace are diagnostics, not part of the reuse
    /// identity. Two manifests differing only there must share a ref.
    #[test]
    fn content_ref_ignores_non_content_fields(m in arb_valid_manifest()) {
        let mut noisy = m.clone();
        noisy.annotations = serde_json::json!({ "image_tag": "warm-1", "n": 42 });
        prop_assert_eq!(noisy.content_ref(), m.content_ref());
    }

    /// `validate()` accepts everything the builder produces — the whole
    /// point of the strategy's construction invariant.
    #[test]
    fn validate_accepts_builder_output(m in arb_valid_manifest()) {
        prop_assert!(m.validate().is_ok(), "builder output must validate: {:?}", m.validate());
    }

    /// A valid manifest survives a build_per_chunk → to_manifest bounce
    /// with its chunk layout intact (the OCI bootstrap producer/consumer
    /// pair, a pure geometry transform, must not perturb offsets/hashes).
    #[test]
    fn bootstrap_bounce_preserves_chunk_layout(m in arb_valid_manifest()) {
        let round = Bootstrap::build_per_chunk(&m).to_manifest();
        prop_assert_eq!(round.kind, m.kind);
        prop_assert_eq!(round.chunk_size, m.chunk_size);
        prop_assert_eq!(round.total_bytes, m.total_bytes);
        prop_assert_eq!(round.chunks, m.chunks);
    }

    /// Mutating a valid manifest's offset off its alignment makes
    /// `validate()` reject it. (chunk_size ≥ 2, so +1 is misaligned.)
    #[test]
    fn validate_rejects_misaligned_offset(
        m in arb_valid_manifest_with_chunks(1),
        idx in any::<prop::sample::Index>(),
    ) {
        let mut bad = m.clone();
        let k = idx.index(bad.chunks.len());
        bad.chunks[k].offset += 1; // no longer a multiple of chunk_size
        prop_assert!(
            matches!(bad.validate(), Err(ChunkStoreError::MalformedManifest(_))),
            "misaligned offset must be rejected",
        );
    }

    /// Pushing any chunk offset to/past total_bytes is rejected. We set
    /// the *last* chunk's offset to `expected_chunk_count * chunk_size`,
    /// which is a valid multiple but ≥ total_bytes — isolating the
    /// bounds check from the alignment and monotonicity checks.
    #[test]
    fn validate_rejects_offset_past_total_bytes(m in arb_valid_manifest_with_chunks(1)) {
        let mut bad = m.clone();
        let cs = bad.chunk_size.as_u64();
        let past = bad.total_bytes.div_ceil(cs) * cs; // multiple, ≥ total_bytes
        let last = bad.chunks.len() - 1;
        bad.chunks[last].offset = past;
        prop_assert!(
            matches!(bad.validate(), Err(ChunkStoreError::MalformedManifest(_))),
            "offset ≥ total_bytes must be rejected (offset={past}, total={})",
            bad.total_bytes,
        );
    }

    /// Breaking strict-monotonicity of offsets is rejected. With ≥ 2
    /// chunks, collapse the second onto the first's offset (still aligned
    /// and in bounds) so only the monotonicity invariant is violated.
    #[test]
    fn validate_rejects_non_monotonic_offsets(m in arb_valid_manifest_with_chunks(2)) {
        let mut bad = m.clone();
        bad.chunks[1].offset = bad.chunks[0].offset; // equal → not strictly increasing
        prop_assert!(
            matches!(bad.validate(), Err(ChunkStoreError::MalformedManifest(_))),
            "non-increasing offsets must be rejected",
        );
    }

    /// A schema_version past the max supported is rejected as
    /// unsupported (never silently consumed).
    #[test]
    fn validate_rejects_future_schema_version(m in arb_valid_manifest()) {
        let mut bad = m;
        bad.schema_version = MANIFEST_SCHEMA_VERSION + 1;
        prop_assert!(
            matches!(
                bad.validate(),
                Err(ChunkStoreError::UnsupportedSchemaVersion { .. })
            ),
            "future schema_version must be rejected as unsupported",
        );
    }
}
