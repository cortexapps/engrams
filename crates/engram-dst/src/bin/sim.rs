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

use engram_dst::{Profile, Sim};

fn main() {
    let mut seed: Option<u64> = None;
    let mut seeds: Option<(u64, u64)> = None;
    let mut steps: u64 = 1500;
    let mut profile = Profile::Chaos;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seed" => seed = args.next().and_then(|v| v.parse().ok()),
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

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    let mut failures = 0u32;
    rt.block_on(async {
        tokio::time::pause();
        for s in range.0..range.1 {
            let mut sim = Sim::new(s, profile);
            match sim.run(steps).await {
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
                    for line in sim.report().trace.iter().rev().take(40).rev() {
                        eprintln!("  {line}");
                    }
                    eprintln!(
                        "replay: cargo run -p engram-dst --release --bin sim -- --seed {s} --steps {steps} --profile {}",
                        match profile {
                            Profile::Calm => "calm",
                            Profile::Chaos => "chaos",
                        }
                    );
                }
            }
        }
    });
    if failures > 0 {
        eprintln!("{failures} seed(s) failed");
        std::process::exit(1);
    }
}
