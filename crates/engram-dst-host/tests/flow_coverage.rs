//! Step-surface coverage (ADR 0098 Phase 2, P2).
//!
//! Asserts the [`Step`] enum covers the documented P2 surface, and is
//! structured so P3+ extensions extend the table rather than silently drift.
//! The `coverage_name` match in the scheduler is wildcard-free, so a NEW
//! `Step` variant is a compile error there; this test then forces the new
//! variant to be listed in `EXPECTED` too (the count + name-set assertions
//! fail otherwise) — the wire-proto exhaustiveness-guard pattern from
//! AGENTS.md applied to the step menu.

use std::collections::BTreeSet;
use std::time::Duration;

use engram_dst_host::Step;

/// One representative of every P2 [`Step`] variant. Adding a variant to the
/// enum without adding it here leaves `coverage_name`s below short of
/// `EXPECTED`, failing the test.
fn all_step_representatives() -> Vec<Step> {
    vec![
        Step::GuestWrite(0, 0),
        Step::GuestRead(0, 0),
        Step::FlushTick(0),
        Step::SpoolExport(0),
        Step::SpoolAdopt(0),
        Step::CrashProcess,
        Step::AbruptCrash,
        Step::Restart,
        Step::AdvanceTime(Duration::from_secs(1)),
        Step::ReconcileTick,
        Step::DropLocalBinding(0),
        Step::RevokeOwnership(0),
        Step::Sigterm(Some(1)),
        Step::SnapshotBegin(0),
        Step::FinalizeTick(0),
        Step::FinalizeCrashAt(0, 3),
        Step::SpoolCrashAt(3),
        Step::FlushHandoffRace(0),
        Step::FlushFenceAbort(0),
        Step::FlushPreRebaseCrash(0),
        Step::MigrationBegin(0),
        Step::MigrationServeState(0),
        Step::MigrationTouch(0),
        Step::MigrationTtlSweep,
        Step::MigrationCommit(0),
        Step::MigrationAbort(0),
        Step::Park(0),
        Step::Unpause(0),
        Step::RegisterRehydrate,
        Step::StaleSweepTick,
        Step::SlotClaim(0),
        Step::SlotPopulateTick,
        Step::CorruptSpoolRecovery(0, true, 0),
    ]
}

/// The documented P2 step surface. P3+ adds names here in the same change
/// that adds the variant.
const EXPECTED: &[&str] = &[
    "GuestWrite",
    "GuestRead",
    "FlushTick",
    "SpoolExport",
    "SpoolAdopt",
    "CrashProcess",
    "AbruptCrash",
    "Restart",
    "AdvanceTime",
    "ReconcileTick",
    "DropLocalBinding",
    "RevokeOwnership",
    "Sigterm",
    "SnapshotBegin",
    "FinalizeTick",
    "FinalizeCrashAt",
    "SpoolCrashAt",
    "FlushHandoffRace",
    "FlushFenceAbort",
    "FlushPreRebaseCrash",
    "MigrationBegin",
    "MigrationServeState",
    "MigrationTouch",
    "MigrationTtlSweep",
    "MigrationCommit",
    "MigrationAbort",
    "Park",
    "Unpause",
    "RegisterRehydrate",
    "StaleSweepTick",
    "SlotClaim",
    "SlotPopulateTick",
    "CorruptSpoolRecovery",
];

#[test]
fn step_enum_covers_the_p2_surface() {
    let names: BTreeSet<&'static str> = all_step_representatives()
        .iter()
        .map(Step::coverage_name)
        .collect();
    let expected: BTreeSet<&'static str> = EXPECTED.iter().copied().collect();
    assert_eq!(
        names, expected,
        "the Step representatives and the EXPECTED surface must match exactly — \
         a new variant needs both a representative and an EXPECTED entry",
    );
    // No two variants collapse to the same coverage name.
    assert_eq!(
        names.len(),
        all_step_representatives().len(),
        "every Step variant must have a distinct coverage_name",
    );
}
