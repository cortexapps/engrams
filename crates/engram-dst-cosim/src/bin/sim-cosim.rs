//! The boundary-swarm CLI (ADR 0098 R-CoSim, rung 2, #784).
//!
//! ```text
//! sim-cosim --seed 42 --steps 600 --profile chaos
//! sim-cosim --seeds 0..40 --steps 600 --profile chaos   # swarm
//! ```
//!
//! Exit 0 = every seed converged with no oracle violation. On failure: prints
//! the seed, the violation, and the trace tail — the failure artifact. Replay =
//! the same binary with the same `--seed`.
//!
//! `--failure-report <path>`: additionally write the failure artifact as a
//! ready-to-file markdown fragment (violation, replay command, trace tail per
//! failed seed, first-slug marker line for issue dedup). Written only when at
//! least one seed fails; the nightly workflow turns it into a GitHub issue. The
//! arg contract mirrors `engram-dst`'s `sim` binary exactly.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_dst_cosim::Profile;

/// Real wall-clock budget for a SINGLE seed. A healthy boundary seed converges
/// in well under a second; anything past this is a LIVENESS hang (an op/driver
/// that never converges, or a wedged paused-clock runtime), not an oracle
/// violation — so the swarm must fail in minutes with the offending seed
/// printed, never sit until a multi-hour CI job timeout. Sized identically to
/// the sibling sims' watchdog.
const PER_SEED_WALL_BUDGET: Duration = Duration::from_secs(600);

/// Spawn the per-seed watchdog on its OWN OS thread (never the paused-clock
/// runtime thread — a wedged runtime can't fire its own timeout). If `done`
/// isn't set within the budget, print the seed + replay line and abort so the
/// lane fails fast.
fn spawn_seed_watchdog(
    seed: u64,
    profile_name: &'static str,
    steps: u64,
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
                     real wall-clock. This is a LIVENESS hang, not an oracle violation. Aborting \
                     so the swarm fails fast instead of hanging CI.\n\
                     replay: cargo run -p engram-dst-cosim --release --bin sim-cosim -- --seed \
                     {seed} --steps {steps} --profile {profile_name}",
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
    let mut steps: u64 = 600;
    let mut profile = Profile::Chaos;
    let mut failure_report: Option<std::path::PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
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
        (None, None) => (0, 40),
        (Some(_), Some(_)) => {
            eprintln!("--seed and --seeds are mutually exclusive");
            std::process::exit(2);
        }
    };

    let profile_name = match profile {
        Profile::Calm => "calm",
        Profile::Chaos => "chaos",
    };

    let mut failures = 0u32;
    let mut report_md = String::new();
    let mut first_slug: Option<String> = None;
    for s in range.0..range.1 {
        let done = Arc::new(AtomicBool::new(false));
        let watchdog = spawn_seed_watchdog(s, profile_name, steps, Arc::clone(&done));

        let outcome = engram_dst_cosim::run_seed(s, profile, steps, 40);

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
                    "replay: cargo run -p engram-dst-cosim --release --bin sim-cosim -- --seed {s} --steps {steps} --profile {profile_name}",
                );
                let slug = msg
                    .split(" — ")
                    .next()
                    .and_then(|head| head.rsplit(": ").next())
                    .unwrap_or("oracle-violation")
                    .to_string();
                first_slug.get_or_insert(slug.clone());
                let _ = write!(
                    report_md,
                    "## `{slug}` — seed {s} ({profile_name}, {steps} steps)\n\n\
                     **Violation:** `{msg}`\n\n\
                     **Replay:**\n```sh\ncargo run -p engram-dst-cosim --release --bin sim-cosim -- --seed {s} --steps {steps} --profile {profile_name}\n```\n\n\
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
            let slug = first_slug.as_deref().unwrap_or("oracle-violation");
            let full = format!("<!-- sim-failure-slug: {slug} -->\n\n{report_md}");
            if let Err(e) = std::fs::write(path, full) {
                eprintln!("could not write --failure-report {}: {e}", path.display());
            }
        }
        eprintln!("{failures} seed(s) failed");
        std::process::exit(1);
    }
}
