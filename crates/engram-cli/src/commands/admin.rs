//! engrams admin … — explicit triggers for primitives whose production driver
//! is implicit (the idle detector, the flush scanner). Same pipelines, no TTL
//! wait.

use serde::Serialize;

use engram_app_client::proto::*;
use engram_app_client::Clients;

use crate::output::{fail, print_json, CliResult};

/// FleetService.FlushSession — force a chunked-disk flush of one session.
pub async fn flush(c: &Clients, id: &str, json: bool) -> CliResult {
    let resp = c
        .flush_session(FlushSessionRequest {
            session_id: id.to_string(),
        })
        .await?;
    if json {
        #[derive(Serialize)]
        struct Out {
            outcome: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            manifest_version: Option<u64>,
        }
        print_json(&Out {
            outcome: resp.outcome,
            manifest_version: resp.manifest_version,
        });
        return Ok(());
    }
    match resp.manifest_version {
        Some(v) => println!("{}: manifest_version={v}", resp.outcome),
        None => println!("{}", resp.outcome),
    }
    Ok(())
}

/// SessionService.EvictIdle — the idle-eviction pipeline, now. Synchronous:
/// the session is Idle by the time this returns.
pub async fn evict_idle(c: &Clients, id: &str, json: bool) -> CliResult {
    let resp = c
        .evict_idle(EvictIdleRequest {
            session_id: id.to_string(),
        })
        .await?;
    if json {
        #[derive(Serialize)]
        struct Out {
            session_id: String,
            status: String,
        }
        print_json(&Out {
            session_id: resp.session_id,
            status: resp.status,
        });
    } else {
        println!("{}: {}", resp.session_id, resp.status);
    }
    Ok(())
}

#[derive(Serialize)]
struct SweepJson {
    listed: u64,
    pinned: u64,
    candidates_marked: u64,
    promoted_deletes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    promote_repinned_skips: Option<u64>,
    promote_delete_errors: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    grace_secs: Option<u64>,
}

#[derive(Serialize)]
struct GcJson {
    dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    bundle: Option<SweepJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_blob: Option<SweepJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunk: Option<SweepJson>,
    failures: Vec<String>,
}

/// The three blob-storage GC sweeps (bundle generations / snapshot blobs /
/// chunks — ADR 0035 §5 / ADR 0028 addendum / ADR 0016 Phase C). The proto's
/// bare `dry_run: false` means a LIVE sweep with deletions, so this verb
/// inverts the polarity: dry-run unless `apply`. `grace_secs == 0` promotes
/// (deletes) candidates in the same sweep — the dev "clear it now" posture.
/// The sweeps are independent: each reports as it lands, failures are
/// collected so one wedged sweep can't swallow the others' reports.
pub async fn gc(c: &Clients, apply: bool, grace_secs: Option<u64>, json: bool) -> CliResult {
    let dry_run = !apply;
    let mode = if dry_run { "dry-run" } else { "applied" };
    let mut failures: Vec<String> = Vec::new();
    let mut out = GcJson {
        dry_run,
        bundle: None,
        snapshot_blob: None,
        chunk: None,
        failures: vec![],
    };

    match c
        .bundle_gc(BundleGcRequest {
            dry_run,
            grace_secs,
        })
        .await
    {
        Ok(b) => {
            if json {
                out.bundle = Some(SweepJson {
                    listed: b.listed,
                    pinned: b.pin_set_size,
                    candidates_marked: b.candidates_marked,
                    promoted_deletes: b.promoted_deletes,
                    promote_repinned_skips: None,
                    promote_delete_errors: b.promote_delete_errors,
                    grace_secs: None,
                });
            } else {
                println!(
                    "bundle-gc ({mode}): listed={} pinned={} candidates={} deleted={} errors={}",
                    b.listed,
                    b.pin_set_size,
                    b.candidates_marked,
                    b.promoted_deletes,
                    b.promote_delete_errors
                );
            }
        }
        Err(e) => failures.push(format!("bundle-gc: {e}")),
    }

    match c
        .snapshot_blob_gc(SnapshotBlobGcRequest {
            dry_run,
            grace_secs,
        })
        .await
    {
        Ok(s) => {
            if json {
                out.snapshot_blob = Some(SweepJson {
                    listed: s.listed,
                    pinned: s.pin_set_size,
                    candidates_marked: s.candidates_marked,
                    promoted_deletes: s.promoted_deletes,
                    promote_repinned_skips: Some(s.promote_repinned_skips),
                    promote_delete_errors: s.promote_delete_errors,
                    grace_secs: None,
                });
            } else {
                println!(
                    "snapshot-blob-gc ({mode}): listed={} pinned={} candidates={} deleted={} repinned_skips={} errors={}",
                    s.listed, s.pin_set_size, s.candidates_marked, s.promoted_deletes, s.promote_repinned_skips, s.promote_delete_errors
                );
            }
        }
        Err(e) => failures.push(format!("snapshot-blob-gc: {e}")),
    }

    match c
        .chunk_gc(ChunkGcRequest {
            dry_run,
            grace_secs,
        })
        .await
    {
        Ok(k) => {
            if json {
                out.chunk = Some(SweepJson {
                    listed: k.listed_chunks,
                    pinned: k.pin_set_size,
                    candidates_marked: k.candidates_marked,
                    promoted_deletes: k.promoted_deletes,
                    promote_repinned_skips: None,
                    promote_delete_errors: k.promote_delete_errors,
                    grace_secs: Some(k.grace_secs),
                });
            } else {
                println!(
                    "chunk-gc ({mode}): listed={} pinned={} candidates={} deleted={} errors={} grace_secs={}",
                    k.listed_chunks, k.pin_set_size, k.candidates_marked, k.promoted_deletes, k.promote_delete_errors, k.grace_secs
                );
            }
        }
        Err(e) => failures.push(format!("chunk-gc: {e}")),
    }

    if json {
        out.failures = failures.clone();
        print_json(&out);
    }
    if let Some(first) = failures.first() {
        for f in &failures[1..] {
            eprintln!("engrams: {f}");
        }
        return fail(first.clone());
    }
    Ok(())
}
