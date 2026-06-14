//! Byte-level golden + variant-index pins for the ADR 0045 C2 migration
//! page channel wire types (`ToSource`, `FromSource`, `HandlerControl`,
//! and the payload structs they carry).
//!
//! ## Why this exists
//!
//! `bincode` (1.x, the framing in `engram-migrate-proto`) is positional:
//! enums encode by `u32` variant *index*, structs by field *order*.
//! Neither is self-describing. The pre-existing round-trip tests in
//! `src/lib.rs` cannot catch a format break — both ends recompile
//! together in CI, so a reordered variant or an inserted field
//! round-trips green and then desyncs against a peer built from an older
//! tree. The dest uffd-handler and the source host-agent roll on
//! independent schedules, so that skew is real.
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
//!   cargo test -p engram-migrate-proto --test wire_golden -- --ignored regen_golden
//! ```
//!
//! then `git add` the changed `golden/*.bin` and review the diff: an
//! EXISTING golden file changing bytes is a RED FLAG (you broke the wire
//! for an old peer); only NEW files (for the new trailing case) are
//! expected. Reordering/inserting is never OK — see the `ToSource` /
//! `FromSource` / `HandlerControl` doc comments in `src/lib.rs`.

use std::path::PathBuf;

use engram_migrate_proto::{
    ConnPurpose, FromSource, HandlerControl, SealBitmap, ToSource, PROTO_VERSION,
};
use serde::Serialize;

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.bin"))
}

/// Assert `value` bincode-encodes to exactly the bytes in
/// `golden/<name>.bin` AND that those golden bytes decode back to an
/// equal value. A mismatch means the wire format changed — see the
/// module header before touching the corpus.
fn assert_golden<T>(name: &str, value: &T)
where
    T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let encoded = bincode::serialize(value).expect("bincode encode");
    let path = golden_path(name);
    let golden = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden file {}: {e}\n\
             regenerate with: cargo test -p engram-migrate-proto --test wire_golden -- --ignored regen_golden",
            path.display()
        )
    });
    assert_eq!(
        encoded, golden,
        "wire format for `{name}` changed: bincode output != golden bytes.\n\
         bincode is POSITIONAL — a reordered field/variant or an inserted \
         (non-trailing) field breaks every peer built from an older tree \
         (the dest uffd-handler and source host-agent roll independently).\n\
         If this change is an intentional, wire-SAFE evolution (a TRAILING \
         field/variant only), regenerate the corpus per the module header."
    );
    let decoded: T = bincode::deserialize(&golden).expect("bincode decode golden");
    assert_eq!(
        &decoded, value,
        "golden bytes for `{name}` no longer decode to the expected value"
    );
}

/// Pin the `u32` variant index bincode writes as the first 4 bytes of an
/// enum encoding. This is the single most fragile thing about the wire:
/// inserting a variant mid-enum shifts every later index and silently
/// aliases (an old peer decodes the new variant as a different one).
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
         Reordering/inserting desyncs every peer built from an older tree."
    );
}

// A fixed bitmap so the golden bytes are deterministic.
fn sample_bitmap() -> SealBitmap {
    let mut b = SealBitmap::new(512 * 1024, 17);
    b.set(0);
    b.set(16);
    b
}

#[test]
fn to_source_golden_and_variant_indices() {
    let hello = ToSource::Hello {
        version: PROTO_VERSION,
        export_id: "ab12".into(),
        token: "secret".into(),
        purpose: ConnPurpose::Fault,
    };
    let need_at = ToSource::NeedAt {
        req_id: 7,
        chunk_offset: 512 * 1024 * 3,
    };
    let get_chunk = ToSource::GetChunk {
        req_id: 8,
        hash: [0xAB; 32],
    };
    let drain_done = ToSource::DrainDone {
        pulled: 100,
        alt_sourced: 3,
        zero_chunks: 42,
    };

    assert_golden("to_source_hello", &hello);
    assert_golden("to_source_need_at", &need_at);
    assert_golden("to_source_get_chunk", &get_chunk);
    assert_golden("to_source_drain_done", &drain_done);

    assert_variant_index(&hello, 0, "ToSource::Hello");
    assert_variant_index(&need_at, 1, "ToSource::NeedAt");
    assert_variant_index(&get_chunk, 2, "ToSource::GetChunk");
    assert_variant_index(&drain_done, 3, "ToSource::DrainDone");
}

#[test]
fn conn_purpose_golden_and_variant_indices() {
    assert_golden("conn_purpose_fault", &ConnPurpose::Fault);
    assert_golden("conn_purpose_drain", &ConnPurpose::Drain);
    assert_variant_index(&ConnPurpose::Fault, 0, "ConnPurpose::Fault");
    assert_variant_index(&ConnPurpose::Drain, 1, "ConnPurpose::Drain");
}

#[test]
fn from_source_golden_and_variant_indices() {
    let hello_ack = FromSource::HelloAck {
        version: PROTO_VERSION,
        chunk_size: 512 * 1024,
        total_bytes: 128 * 1024 * 1024,
    };
    let seal = FromSource::Seal {
        bitmap: sample_bitmap(),
    };
    let page = FromSource::Page {
        req_id: 1,
        chunk_offset: 0,
        bytes: vec![0xCD; 64],
        hash: [0x11; 32],
        lz4: false,
    };
    let zero_chunk = FromSource::ZeroChunk {
        req_id: 2,
        chunk_offset: 512 * 1024,
    };
    let alt_source = FromSource::AltSource {
        req_id: 3,
        chunk_offset: 1024 * 1024,
        durable_sha256: [0x22; 32],
    };
    let chunk_bytes = FromSource::ChunkBytes {
        req_id: 4,
        bytes: vec![1, 2, 3],
    };
    let error = FromSource::Error {
        req_id: None,
        message: "bad hello".into(),
    };

    assert_golden("from_source_hello_ack", &hello_ack);
    assert_golden("from_source_seal", &seal);
    assert_golden("from_source_page", &page);
    assert_golden("from_source_zero_chunk", &zero_chunk);
    assert_golden("from_source_alt_source", &alt_source);
    assert_golden("from_source_chunk_bytes", &chunk_bytes);
    assert_golden("from_source_error", &error);

    assert_variant_index(&hello_ack, 0, "FromSource::HelloAck");
    assert_variant_index(&seal, 1, "FromSource::Seal");
    assert_variant_index(&page, 2, "FromSource::Page");
    assert_variant_index(&zero_chunk, 3, "FromSource::ZeroChunk");
    assert_variant_index(&alt_source, 4, "FromSource::AltSource");
    assert_variant_index(&chunk_bytes, 5, "FromSource::ChunkBytes");
    assert_variant_index(&error, 6, "FromSource::Error");
}

#[test]
fn handler_control_golden_and_variant_indices() {
    let sealed = HandlerControl::Sealed {
        dirty_chunks: 12,
        total_chunks: 256,
        at_unix_ms: 1_770_000_000_000,
    };
    let drain_progress = HandlerControl::DrainProgress {
        pulled: 6,
        remaining: 6,
    };
    let drain_done = HandlerControl::DrainDone {
        pulled: 10,
        alt_sourced: 1,
        zero_chunks: 1,
        ms: 1234,
        faults: 42,
        fault_us: 55_000,
        fault_max_us: 9_000,
    };
    let peer_lost = HandlerControl::PeerLost {
        remaining: 3,
        detail: "connection reset".into(),
    };

    assert_golden("handler_control_sealed", &sealed);
    assert_golden("handler_control_drain_progress", &drain_progress);
    assert_golden("handler_control_drain_done", &drain_done);
    assert_golden("handler_control_peer_lost", &peer_lost);

    assert_variant_index(&sealed, 0, "HandlerControl::Sealed");
    assert_variant_index(&drain_progress, 1, "HandlerControl::DrainProgress");
    assert_variant_index(&drain_done, 2, "HandlerControl::DrainDone");
    assert_variant_index(&peer_lost, 3, "HandlerControl::PeerLost");
}

#[test]
fn seal_bitmap_payload_golden() {
    // `SealBitmap` is a struct (no variant index) but it rides
    // `FromSource::Seal` across the wire; pin its standalone encoding so
    // a field reorder is caught even if the enum framing is unchanged.
    assert_golden("seal_bitmap", &sample_bitmap());
}

/// Writer for the golden corpus. `#[ignore]`d so a normal `cargo test`
/// never regenerates (which would mask a real break). See module header.
#[test]
#[ignore = "regenerates the golden corpus; run explicitly when intentionally evolving a wire type"]
fn regen_golden() {
    fn write<T: Serialize>(name: &str, value: &T) {
        let bytes = bincode::serialize(value).expect("bincode encode");
        let path = golden_path(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        eprintln!("wrote {} ({} bytes)", path.display(), bytes.len());
    }

    write(
        "to_source_hello",
        &ToSource::Hello {
            version: PROTO_VERSION,
            export_id: "ab12".into(),
            token: "secret".into(),
            purpose: ConnPurpose::Fault,
        },
    );
    write(
        "to_source_need_at",
        &ToSource::NeedAt {
            req_id: 7,
            chunk_offset: 512 * 1024 * 3,
        },
    );
    write(
        "to_source_get_chunk",
        &ToSource::GetChunk {
            req_id: 8,
            hash: [0xAB; 32],
        },
    );
    write(
        "to_source_drain_done",
        &ToSource::DrainDone {
            pulled: 100,
            alt_sourced: 3,
            zero_chunks: 42,
        },
    );

    write("conn_purpose_fault", &ConnPurpose::Fault);
    write("conn_purpose_drain", &ConnPurpose::Drain);

    write(
        "from_source_hello_ack",
        &FromSource::HelloAck {
            version: PROTO_VERSION,
            chunk_size: 512 * 1024,
            total_bytes: 128 * 1024 * 1024,
        },
    );
    write(
        "from_source_seal",
        &FromSource::Seal {
            bitmap: sample_bitmap(),
        },
    );
    write(
        "from_source_page",
        &FromSource::Page {
            req_id: 1,
            chunk_offset: 0,
            bytes: vec![0xCD; 64],
            hash: [0x11; 32],
            lz4: false,
        },
    );
    write(
        "from_source_zero_chunk",
        &FromSource::ZeroChunk {
            req_id: 2,
            chunk_offset: 512 * 1024,
        },
    );
    write(
        "from_source_alt_source",
        &FromSource::AltSource {
            req_id: 3,
            chunk_offset: 1024 * 1024,
            durable_sha256: [0x22; 32],
        },
    );
    write(
        "from_source_chunk_bytes",
        &FromSource::ChunkBytes {
            req_id: 4,
            bytes: vec![1, 2, 3],
        },
    );
    write(
        "from_source_error",
        &FromSource::Error {
            req_id: None,
            message: "bad hello".into(),
        },
    );

    write(
        "handler_control_sealed",
        &HandlerControl::Sealed {
            dirty_chunks: 12,
            total_chunks: 256,
            at_unix_ms: 1_770_000_000_000,
        },
    );
    write(
        "handler_control_drain_progress",
        &HandlerControl::DrainProgress {
            pulled: 6,
            remaining: 6,
        },
    );
    write(
        "handler_control_drain_done",
        &HandlerControl::DrainDone {
            pulled: 10,
            alt_sourced: 1,
            zero_chunks: 1,
            ms: 1234,
            faults: 42,
            fault_us: 55_000,
            fault_max_us: 9_000,
        },
    );
    write(
        "handler_control_peer_lost",
        &HandlerControl::PeerLost {
            remaining: 3,
            detail: "connection reset".into(),
        },
    );

    write("seal_bitmap", &sample_bitmap());
}
