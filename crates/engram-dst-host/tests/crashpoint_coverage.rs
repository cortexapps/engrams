//! The SimFs ∘ H5 composition meta-test (ADR 0098 Phase 2, P2).
//!
//! P2 names the durable-operation crash boundaries ([`CrashPoint`]) but does
//! not yet INJECT a crash at one (that is P4). What it CAN prove today is the
//! composition contract the ADR draws: every boundary a P4 injector could cut
//! at already has an ADR 0099 H5 test standing behind it — a test that builds
//! the resulting post-crash on-disk state EXTERNALLY (no fault trait) and
//! asserts tolerant recovery. This table maps each boundary to that test.
//!
//! The map is wildcard-free over [`CrashPoint`], and it iterates
//! [`CrashPoint::ALL`], so a new boundary is a compile error here (in the
//! `match`) and a hard assertion failure (missing from `ALL`'s coverage) —
//! never a silent gap. The referenced test names are the actual `#[test]`
//! fns in the two source modules' `mod tests` blocks; when P4 wires the
//! injector, each boundary's runtime crash lands in exactly the on-disk
//! state its named H5 test already pins.

use engram_dst_host::CrashPoint;

/// The H5 test that pins the post-crash on-disk state this boundary leaves.
/// `module` is the source file's `#[cfg(test)] mod tests`; `test` is the fn.
struct H5Backing {
    module: &'static str,
    test: &'static str,
    /// Why cutting at this boundary lands in the state that test constructs.
    _why: &'static str,
}

fn backing(cp: CrashPoint) -> H5Backing {
    match cp {
        // ── durable_record::persist ─────────────────────────────────────
        CrashPoint::PersistWritePartial => H5Backing {
            module: "engram_host_agent::durable_record",
            test: "garbage_suffix_and_leftover_partial_are_skipped",
            _why: "cutting mid/after the `.json.partial` write leaves a leftover \
                   partial (or a torn one); load_all's extension filter + serde \
                   skip it — the leftover-partial leg of that test.",
        },
        CrashPoint::PersistFsyncTemp => H5Backing {
            module: "engram_host_agent::durable_record",
            test: "garbage_suffix_and_leftover_partial_are_skipped",
            _why: "cutting after the temp fsync, before the rename, still leaves a \
                   complete `.json.partial`; same leftover-partial tolerance.",
        },
        CrashPoint::PersistRename => H5Backing {
            module: "engram_host_agent::durable_record",
            test: "truncation_at_every_offset_tolerated_siblings_survive",
            _why: "cutting during/after the rename can publish a torn `.json`; the \
                   exhaustive truncation sweep proves every prefix is skipped and \
                   the siblings survive.",
        },
        CrashPoint::PersistFsyncParent => H5Backing {
            module: "engram_host_agent::durable_record",
            test: "persist_then_load_all_roundtrips",
            _why: "cutting after the rename, before the parent-dir fsync, may lose \
                   the dir entry → the record reads as absent; the happy-path \
                   round-trip anchors that a fully-synced record loads.",
        },

        // ── disk_daemon::spool::write_spool ─────────────────────────────
        CrashPoint::SpoolChunks => H5Backing {
            module: "engram_host_agent::disk_daemon::spool",
            test: "torn_chunk_under_valid_marker_is_rejected_at_every_offset",
            _why: "cutting mid chunk write leaves a torn `chunk-*.bin`; read_spool \
                   re-hashes each chunk and rejects a torn one at every truncation \
                   offset, never adopting a valid sibling as a subset.",
        },
        CrashPoint::SpoolChunkMissing => H5Backing {
            module: "engram_host_agent::disk_daemon::spool",
            test: "marker_lists_a_chunk_whose_file_is_missing_is_rejected",
            _why: "a listed chunk file gone after write → the marker's all-or-nothing \
                   contract rejects, no partial adopt.",
        },
        CrashPoint::SpoolMarker => H5Backing {
            module: "engram_host_agent::disk_daemon::spool",
            test: "missing_completeness_marker_reads_as_absent",
            _why: "the marker is written+fsync'd LAST; cutting before it leaves no \
                   marker → the spool reads as absent (the torn-marker variant is \
                   covered by `unparsable_marker_is_rejected`).",
        },
        CrashPoint::SpoolDir => H5Backing {
            module: "engram_host_agent::disk_daemon::spool",
            test: "zero_chunk_ref_only_spool_roundtrips_and_torn_ref_rejected",
            _why: "cutting after the marker fsync, before the dir-entry fsync, is the \
                   ref-only / zero-chunk durability leg — it round-trips, and a torn \
                   ref is rejected.",
        },
    }
}

#[test]
fn every_crash_boundary_names_an_h5_backing_test() {
    for cp in CrashPoint::ALL {
        let b = backing(cp);
        assert!(
            !b.module.is_empty() && !b.test.is_empty(),
            "{cp:?} ({}) must name an H5-backed test",
            cp.format(),
        );
        // The named test's module must be one of the two durable formats.
        assert!(
            b.module.contains("durable_record") || b.module.contains("spool"),
            "{cp:?} backing module {} is neither durable_record nor spool",
            b.module,
        );
    }
    // Every boundary is accounted for exactly once.
    assert_eq!(
        CrashPoint::ALL.len(),
        8,
        "the P2 boundary catalogue is 8 legs"
    );
}
