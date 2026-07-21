//! The simulator CLI (ADR 0098 D7).
//!
//! ```text
//! sim --seed 42 --steps 3000 --profile chaos
//! sim --seeds 0..200 --steps 1500 --profile chaos   # swarm
//! ```
//!
//! Exit 0 = every seed converged with no invariant violation. On
//! failure: prints the seed, the violation, and the trace tail — the
//! failure artifact. Replay = the same binary with the same --seed.
//!
//! `--failure-report <path>`: additionally write the failure artifact
//! as a ready-to-file markdown fragment (violation, replay command,
//! trace tail per failed seed, first-slug marker line for issue
//! dedup). Written only when at least one seed fails; the nightly
//! workflow turns it into a GitHub issue.

use engram_dst::Profile;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Real wall-clock budget for a SINGLE seed. A healthy seed converges in
/// well under a second (the whole 200-seed window is ~5 min), so a single
/// seed taking longer than the entire normal lane is unambiguously a hang.
/// Its ONLY job is to convert a genuine liveness hang (an op/driver that
/// never converges, or a wedged paused-clock runtime) into a FAST, NAMED
/// failure — a multi-hour CI hang is the worst possible reporting mode, so
/// the swarm must fail in minutes with the offending seed printed, never
/// sit until a 6 h job timeout.
///
/// Sized well ABOVE the slowest a HEALTHY seed can run even under
/// pathological CPU starvation — a heavy chaos seed pinned to 1/7 of one
/// core (6 CPU hogs + the sim on the same core) measured ~210 s real, its
/// many paused-clock park/auto-advance cycles each waiting on an OS
/// reschedule — so this never false-trips a merely-slow seed, only a true
/// hang (which parks the runtime forever and trips any finite budget). No
/// real CI runner is that contended; the margin is the point.
const PER_SEED_WALL_BUDGET: Duration = Duration::from_secs(600);

/// Spawn the per-seed wall-clock watchdog on its OWN OS thread (never the
/// paused-clock runtime thread — a wedged runtime can't fire its own
/// timeout). If `done` is not set within the budget, print the seed + its
/// replay line and abort the whole process so the lane fails fast.
fn spawn_seed_watchdog(
    seed: u64,
    profile_name: &'static str,
    steps: u64,
    faithful_flag: &'static str,
    done: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let deadline = Instant::now() + PER_SEED_WALL_BUDGET;
        loop {
            if done.load(Ordering::SeqCst) {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                eprintln!(
                    "seed {seed}: WATCHDOG TIMEOUT — no verdict within {PER_SEED_WALL_BUDGET:?} of \
                     real wall-clock. This is a LIVENESS hang (an op/driver that never converges, \
                     or a wedged paused-clock runtime), not an invariant violation. Aborting so \
                     the swarm fails fast instead of hanging CI.\n\
                     replay: cargo run -p engram-dst --release --bin sim -- --seed {seed} --steps \
                     {steps} --profile {profile_name}{faithful_flag}",
                );
                std::process::exit(1);
            }
            std::thread::park_timeout(deadline - now);
        }
    })
}

fn main() {
    let mut seed: Option<u64> = None;
    let mut seeds: Option<(u64, u64)> = None;
    let mut steps: u64 = 1500;
    let mut profile = Profile::Chaos;
    let mut failure_report: Option<std::path::PathBuf> = None;
    // R3: faithful hosts are now the swarm DEFAULT. The two classes that
    // blocked the flip are fixed — evict→resume `snapshot-safety` (#790) and
    // `placement-accounting` (#722, this change: one reservation authority) —
    // so every swarm seed exercises the schedulable, digest-gated
    // `candidates_for` path the coordinator actually runs. `--no-faithful`
    // opts back to the legacy non-schedulable world for ad-hoc bisection.
    let mut faithful = true;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--faithful" => faithful = true,
            "--no-faithful" => faithful = false,
            "--seed" => seed = args.next().and_then(|v| v.parse().ok()),
            "--failure-report" => failure_report = args.next().map(std::path::PathBuf::from),
            "--seeds" => {
                seeds = args.next().and_then(|v| {
                    let (lo, hi) = v.split_once("..")?;
                    Some((lo.parse().ok()?, hi.parse().ok()?))
                })
            }
            "--steps" => steps = args.next().and_then(|v| v.parse().ok()).unwrap_or(steps),
            "--profile" => {
                profile = match args.next().as_deref() {
                    Some("calm") => Profile::Calm,
                    Some("chaos") | None => Profile::Chaos,
                    Some(other) => {
                        eprintln!("unknown profile `{other}` (calm|chaos)");
                        std::process::exit(2);
                    }
                }
            }
            other => {
                eprintln!("unknown arg `{other}`");
                std::process::exit(2);
            }
        }
    }
    let range = match (seed, seeds) {
        (Some(s), None) => (s, s + 1),
        (None, Some(r)) => r,
        (None, None) => (0, 50),
        (Some(_), Some(_)) => {
            eprintln!("--seed and --seeds are mutually exclusive");
            std::process::exit(2);
        }
    };

    let profile_name = match profile {
        Profile::Calm => "calm",
        Profile::Chaos => "chaos",
    };
    // Faithful is the default now, so a replay only needs a flag to REPRODUCE
    // the non-faithful case.
    let faithful_flag = if faithful { "" } else { " --no-faithful" };

    let mut failures = 0u32;
    let mut report_md = String::new();
    let mut first_slug: Option<String> = None;
    // Every seed runs on its OWN runtime (`engram_dst::run_seed`), dropped
    // before the next — so no detached task leaks across the boundary (the
    // paused-clock cross-seed deadlock that hung this lane; see `run_seed`).
    // A per-seed wall-clock watchdog bounds any residual/future liveness
    // hang to a fast, named failure instead of a multi-hour CI hang.
    for s in range.0..range.1 {
        let done = Arc::new(AtomicBool::new(false));
        let watchdog =
            spawn_seed_watchdog(s, profile_name, steps, faithful_flag, Arc::clone(&done));

        let outcome = engram_dst::run_seed(s, profile, steps, faithful, 40);

        done.store(true, Ordering::SeqCst);
        watchdog.thread().unpark();
        let _ = watchdog.join();

        match outcome.result {
            Ok(report) => {
                println!(
                    "seed {s}: OK ({} steps, {} sessions)",
                    report.steps_run, report.sessions_created
                );
            }
            Err(msg) => {
                failures += 1;
                eprintln!("seed {s}: FAILED — {msg}");
                eprintln!("--- trace tail (last 40 steps) ---");
                for line in &outcome.trace {
                    eprintln!("  {line}");
                }
                eprintln!(
                    "replay: cargo run -p engram-dst --release --bin sim -- --seed {s} --steps {steps} --profile {profile_name}{faithful_flag}",
                );
                // Failure messages are `step N: <invariant> — <detail>`
                // or `quiescence: <invariant> — <detail>`; the token
                // before the em-dash is the stable invariant name.
                let slug = msg
                    .split(" — ")
                    .next()
                    .and_then(|head| head.rsplit(": ").next())
                    .unwrap_or("invariant-violation")
                    .to_string();
                first_slug.get_or_insert(slug.clone());
                let _ = write!(
                    report_md,
                    "## `{slug}` — seed {s} ({profile_name}, {steps} steps)\n\n\
                     **Violation:** `{msg}`\n\n\
                     **Replay:**\n```sh\njust sim SEED={s} STEPS={steps} PROFILE={profile_name}\n```\n\n\
                     <details><summary>Trace tail (last 40 steps)</summary>\n\n```text\n",
                );
                for line in &outcome.trace {
                    let _ = writeln!(report_md, "{line}");
                }
                let _ = write!(report_md, "```\n\n</details>\n\n");
            }
        }
    }
    if failures > 0 {
        if let Some(path) = &failure_report {
            let slug = first_slug.as_deref().unwrap_or("invariant-violation");
            let full = format!("<!-- sim-failure-slug: {slug} -->\n\n{report_md}");
            if let Err(e) = std::fs::write(path, full) {
                eprintln!("could not write --failure-report {}: {e}", path.display());
            }
        }
        eprintln!("{failures} seed(s) failed");
        std::process::exit(1);
    }
}
