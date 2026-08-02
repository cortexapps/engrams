//! The first end-to-end simulations (ADR 0098 D5).
//!
//! Fixed seeds — a CI failure is always locally reproducible with
//! `Sim::new(<seed>, <profile>).run(<steps>)`. Every seed that finds a
//! real bug gets pinned in tests/regression_seeds.rs with a comment
//! naming the fix.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use engram_dst::{DriverKind, Profile, Sim};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime")
}

/// Calm profile: workload + drivers, no faults. The liveness baseline —
/// every session must reach a stable state at quiescence.
///
/// Each seed runs on its OWN runtime via `engram_dst::run_seed` (dropped
/// before the next), so no detached task leaks across the seed boundary —
/// the hermetic seed independence the swarm binary also relies on (a
/// shared runtime across seeds is the cross-seed paused-clock deadlock).
#[test]
fn calm_seeds_converge() {
    for seed in 0..24u64 {
        let outcome = engram_dst::run_seed(seed, Profile::Calm, 400, false, 25);
        match outcome.result {
            Ok(report) => assert_eq!(report.steps_run, 400),
            Err(msg) => panic!(
                "calm seed {seed} violated an invariant: {msg}\nlast trace:\n{}",
                outcome.trace.join("\n"),
            ),
        }
    }
}

/// Chaos profile: host crashes/restarts, replica crashes/restarts, PG
/// outage windows — the system must still converge once faults heal.
/// Per-seed hermetic runtime as in `calm_seeds_converge`.
#[test]
fn chaos_seeds_converge() {
    for seed in 0..24u64 {
        let outcome = engram_dst::run_seed(seed, Profile::Chaos, 600, false, 25);
        if let Err(msg) = outcome.result {
            panic!(
                "chaos seed {seed} violated an invariant: {msg}\nlast trace:\n{}",
                outcome.trace.join("\n"),
            );
        }
    }
}

/// THE determinism contract: the same seed produces byte-identical
/// traces on two independent runs. This is the replay-twice-and-diff
/// self-check from the ADR — the guard against detached-spawn /
/// HashMap-iteration / unbiased-select leaks making seeds
/// non-replayable (the most demoralizing DST failure mode).
#[test]
fn same_seed_replays_identically() {
    let run = |seed: u64| {
        let rt = rt();
        rt.block_on(async {
            tokio::time::pause();
            let mut sim = Sim::new(seed, Profile::Chaos);
            match sim.run(500).await {
                Ok(r) => r.trace,
                Err(_) => std::mem::take(&mut sim.report_mut().trace),
            }
        })
    };
    for seed in [3u64, 7, 42] {
        let a = run(seed);
        let b = run(seed);
        assert_eq!(a.len(), b.len(), "seed {seed}: trace lengths diverged");
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            assert_eq!(x, y, "seed {seed}: traces diverged at step {i}");
        }
    }
}

/// Distinct seeds explore distinct interleavings (a degenerate PRNG or
/// a swallowed seed would silently collapse the swarm's coverage).
#[test]
fn distinct_seeds_diverge() {
    let run = |seed: u64| {
        let rt = rt();
        rt.block_on(async {
            tokio::time::pause();
            let mut sim = Sim::new(seed, Profile::Chaos);
            match sim.run(200).await {
                Ok(r) => r.trace,
                Err(_) => std::mem::take(&mut sim.report_mut().trace),
            }
        })
    };
    assert_ne!(run(1), run(2), "different seeds must explore differently");
}

/// D6's full `run_once` inventory assertion: every coordinator driver
/// entry point is either represented by `DriverKind` or carries an
/// explicit, justified simulation exclusion (ADR 0098).
#[test]
fn driver_coverage_is_declared() {
    const ALL: &[DriverKind] = &[
        DriverKind::QueueScanner,
        DriverKind::Reconcile,
        DriverKind::DeadHost,
        DriverKind::SessionOps,
        DriverKind::OpReclaim,
        DriverKind::IdleDetector,
        DriverKind::IdleEvictor,
        DriverKind::EvacResumer,
        DriverKind::EnableScanner,
        DriverKind::CheckpointRetention,
        DriverKind::BaseSnapshotRetention,
        DriverKind::OutboxDelivery,
        DriverKind::GcSweeps,
    ];
    const EXCLUDED: &[(&str, &str)] = &[
        (
            "chunk_gc",
            "pin-set mark pass spans the manifest/chunk data plane (7 unimplemented SimMeta list_* surfaces plus real manifest blobs); this is an open SimMeta deviation per ADR 0098, while bundle_gc and snapshot_blob_gc carry the GC row/lease semantics the sim can honestly drive",
        ),
        (
            "harness_desync",
            "not a timer driver, so no DriverKind: production runs run_once inside the host-heartbeat handler (api/host_http.rs), and the sim mirrors that exactly — Step::HostHeartbeats drives it with the world-derived running/attached sets right after touch_host_heartbeat (ADR 0108 A8; pinned in tests/ttft_attach.rs)",
        ),
    ];

    fn modules_for(kind: DriverKind) -> &'static [&'static str] {
        match kind {
            DriverKind::QueueScanner => &["queue_scanner"],
            DriverKind::Reconcile => &[],
            DriverKind::DeadHost => &["dead_host"],
            DriverKind::SessionOps => &[],
            DriverKind::OpReclaim => &[],
            DriverKind::IdleDetector => &["idle_detector"],
            DriverKind::IdleEvictor => &["idle_evictor"],
            DriverKind::EvacResumer => &["evac_resumer"],
            DriverKind::EnableScanner => &["enable_scanner"],
            DriverKind::CheckpointRetention => &["checkpoint_retention"],
            DriverKind::BaseSnapshotRetention => &["base_snapshot_retention"],
            DriverKind::OutboxDelivery => &[],
            DriverKind::GcSweeps => &["bundle_gc", "snapshot_blob_gc"],
        }
    }

    fn contains_driver_entry_point(source: &str) -> bool {
        source.lines().any(|line| {
            let line = line.trim();
            let name = [
                "pub async fn ",
                "pub(crate) async fn ",
                "pub fn ",
                "pub(crate) fn ",
            ]
            .iter()
            .find_map(|prefix| line.strip_prefix(prefix))
            .map(|rest| {
                rest.chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<String>()
            });

            name.is_some_and(|name| {
                !name.ends_with("_inner")
                    && (name == "run_once"
                        || name == "scanner_run_once"
                        || name.starts_with("run_one_"))
            })
        })
    }

    fn scan(root: &Path, dir: &Path, inventory: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", dir.display()))
        {
            let path = entry
                .expect("failed to read coordinator source entry")
                .path();
            if path.is_dir() {
                scan(root, &path, inventory);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
                if contains_driver_entry_point(&source) {
                    let relative = path.strip_prefix(root).expect("source path below root");
                    let mut module = relative.with_extension("");
                    if module.file_name().is_some_and(|name| name == "mod") {
                        module = module.parent().unwrap_or(Path::new("")).to_path_buf();
                    }
                    inventory.insert(
                        module
                            .components()
                            .map(|component| component.as_os_str().to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("/"),
                    );
                }
            }
        }
    }

    let root = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../engram-coordinator/src"
    ));
    let mut inventory = BTreeSet::new();
    scan(&root, &root, &mut inventory);
    assert!(
        !inventory.is_empty(),
        "coordinator driver inventory is empty; check the CARGO_MANIFEST_DIR-relative source path"
    );

    let covered: BTreeSet<String> = ALL
        .iter()
        .flat_map(|kind| modules_for(*kind).iter().map(|module| (*module).to_owned()))
        .collect();
    let excluded: BTreeSet<String> = EXCLUDED
        .iter()
        .map(|(module, _)| (*module).to_owned())
        .collect();

    let overlap: Vec<_> = covered.intersection(&excluded).cloned().collect();
    assert!(
        overlap.is_empty(),
        "driver modules cannot be both covered and excluded: {overlap:?}"
    );
    for module in covered.union(&excluded) {
        assert!(
            inventory.contains(module),
            "declared driver module {module:?} is absent from the coordinator run_once inventory; remove or update the stale mapping/exclusion"
        );
    }

    let declared: BTreeSet<_> = covered.union(&excluded).cloned().collect();
    let missing: Vec<_> = inventory.difference(&declared).cloned().collect();
    assert!(
        missing.is_empty(),
        "coordinator driver modules lack simulation coverage: {missing:?}; join each driver to DriverKind plus scheduler wiring, or add an EXCLUDED entry with a one-line justification"
    );
}

/// The interestingness guard: across a seed batch, host death must
/// FEED the recovery ladder, not drain it — some sessions traverse
/// HostLost -> Idle (checkpointed sessions surviving their host), and
/// some Idle sessions get resumed. If this goes dark, chaos sims have
/// regressed into an absorbing everything-dies funnel and the
/// recovery-side code is no longer being exercised (the concern that
/// motivated modeling the checkpoint uploader as world behavior).
#[test]
fn host_death_feeds_the_recovery_ladder() {
    use engram_core::types::session::SessionState;
    let mut hostlost_to_idle = 0u32;
    let mut resume_ops = 0u32;
    // Per-seed hermetic runtime (see `calm_seeds_converge`): this test
    // inspects the world AFTER each run, so it drives `Sim` directly on a
    // fresh runtime per seed rather than through `run_seed`.
    for seed in 0..16u64 {
        let rt = rt();
        rt.block_on(async {
            tokio::time::pause();
            let mut sim = Sim::new(seed, Profile::Chaos);
            let _ = sim.run(600).await;
            sim.world.meta.with_db(|db| {
                hostlost_to_idle += db
                    .transition_log
                    .iter()
                    .filter(|e| e.from == SessionState::HostLost && e.to == SessionState::Idle)
                    .count() as u32;
                resume_ops += db
                    .session_ops
                    .values()
                    .filter(|o| o.kind == engram_core::types::session_op::OpKind::Resume)
                    .count() as u32;
            });
        });
        // `rt` drops here, reaping this seed's detached tasks.
    }
    assert!(
        hostlost_to_idle > 0,
        "no session traversed HostLost -> Idle across 16 chaos seeds — \
         host death has become an absorbing funnel (checkpoint modeling broken?)"
    );
    assert!(
        resume_ops > 0,
        "no Resume op was ever enqueued across 16 chaos seeds — \
         the resume workload is dark"
    );
}
