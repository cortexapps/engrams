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

use engram_dst::{Profile, Sim};
use std::fmt::Write as _;

fn main() {
    let mut seed: Option<u64> = None;
    let mut seeds: Option<(u64, u64)> = None;
    let mut steps: u64 = 1500;
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

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    let mut failures = 0u32;
    let mut report_md = String::new();
    let mut first_slug: Option<String> = None;
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
                        "replay: cargo run -p engram-dst --release --bin sim -- --seed {s} --steps {steps} --profile {profile_name}",
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
                    for line in sim.report().trace.iter().rev().take(40).rev() {
                        let _ = writeln!(report_md, "{line}");
                    }
                    let _ = write!(report_md, "```\n\n</details>\n\n");
                }
            }
        }
    });
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
