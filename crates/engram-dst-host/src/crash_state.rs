//! Externally-constructed post-crash on-disk states (ADR 0098 Phase 2, P4).
//!
//! The seeded crash injector ([`SimHost::crash_at`](crate::world::SimHost::crash_at))
//! cuts the process at one of the eight [`CrashPoint`] durable-operation
//! boundaries. Per the ADR's **H5 composition contract**, SimFs owns
//! reachability at operation granularity while the exhaustively-constructed
//! byte-level torn states stay in ADR 0099 H5's static tests. This module is
//! the live bridge: it builds the SAME post-crash on-disk state each boundary
//! leaves — mirroring the constructors in `spool.rs` / `durable_record.rs`
//! `#[cfg(test)] mod tests` — so a scheduler-driven crash lands in exactly the
//! state its H5 test already pins, and the REAL recovery path
//! (`read_spool` / `load_all`) runs against it under the acked-write oracle.
//!
//! **Determinism:** the chunk a mangle targets is chosen by the SMALLEST
//! parsed `chunk-<idx>.bin` index (never `read_dir` order), so an injection is
//! a pure function of the on-disk set.

use std::path::Path;

use engram_core::SandboxId;

use crate::simfs::CrashPoint;

/// Mangle a COMPLETE spool for `sandbox_id` into the post-crash on-disk state
/// that `cp` (a spool boundary) leaves. The caller must have written a
/// complete, chunk-bearing spool first (via `spool::write_spool`). The four
/// spool boundaries:
///
/// * [`SpoolChunks`] — mid chunk writes, no marker yet: drop `meta.json` and
///   truncate the lowest chunk file. `read_spool` reads it as **absent**
///   (no marker).
/// * [`SpoolChunkMissing`] — a listed chunk file gone after write: remove the
///   lowest chunk, keep the marker. `read_spool` **rejects** (all-or-nothing).
/// * [`SpoolMarker`] — before/mid the marker write: drop `meta.json`.
///   `read_spool` reads it as **absent**.
/// * [`SpoolDir`] — after the marker fsync, before the dir-entry fsync: the
///   fully durable spool. Leave it complete; `read_spool` **adopts** it.
///
/// [`SpoolChunks`]: CrashPoint::SpoolChunks
/// [`SpoolChunkMissing`]: CrashPoint::SpoolChunkMissing
/// [`SpoolMarker`]: CrashPoint::SpoolMarker
/// [`SpoolDir`]: CrashPoint::SpoolDir
pub async fn mangle_spool(
    spool_root: &Path,
    sandbox_id: SandboxId,
    cp: CrashPoint,
) -> Result<(), String> {
    let dir = spool_root.join(sandbox_id.to_string());
    match cp {
        CrashPoint::SpoolChunks => {
            remove_file(&dir.join("meta.json"))?;
            truncate_lowest_chunk(&dir)?;
        }
        CrashPoint::SpoolChunkMissing => {
            remove_lowest_chunk(&dir)?;
        }
        CrashPoint::SpoolMarker => {
            remove_file(&dir.join("meta.json"))?;
        }
        CrashPoint::SpoolDir => {
            // The fully durable spool round-trips — leave it untouched.
        }
        CrashPoint::PersistWritePartial
        | CrashPoint::PersistFsyncTemp
        | CrashPoint::PersistRename
        | CrashPoint::PersistFsyncParent => {
            return Err(format!("mangle_spool: {cp:?} is not a spool boundary"));
        }
    }
    Ok(())
}

/// Construct a `durable_record` boundary state in `records_dir` and assert the
/// REAL `load_all` recovery tolerates it: the valid sibling survives, the torn
/// record never surfaces, nothing panics — the live analog of the H5 static
/// tests. Cleans up after itself so repeated injections start fresh.
///
/// The `durable_record` format backs eviction-finalize / checkpoint (Flow D),
/// which P4 does NOT yet wire into the acked-write ledger — so a persist
/// boundary carries no acked-write payload here; it proves reachability +
/// tolerant recovery, and P5 folds it into the oracle when Flow D routes
/// through the ledger.
pub async fn persist_boundary_recovers(records_dir: &Path, cp: CrashPoint) -> Result<(), String> {
    use engram_host_agent::durable_record;

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Debug, Clone)]
    struct SimRec {
        id: String,
        payload: String,
    }

    let survivor = SimRec {
        id: "survivor".to_string(),
        payload: "x".repeat(64),
    };
    durable_record::persist(records_dir, &survivor.id, &survivor, "sim-rec")
        .await
        .map_err(|e| format!("persist survivor: {e}"))?;

    let torn = SimRec {
        id: "torn".to_string(),
        payload: "y".repeat(256),
    };
    let full = serde_json::to_vec_pretty(&torn).map_err(|e| format!("serialize torn: {e}"))?;
    let half = &full[..full.len() / 2];
    match cp {
        CrashPoint::PersistWritePartial => {
            // Crash mid `.json.partial` write → a torn leftover partial.
            write_raw(&records_dir.join("torn.json.partial"), half).await?;
        }
        CrashPoint::PersistFsyncTemp => {
            // Crash after temp fsync, before rename → a COMPLETE `.json.partial`.
            write_raw(&records_dir.join("torn.json.partial"), &full).await?;
        }
        CrashPoint::PersistRename => {
            // Crash during/after rename → a torn `.json` published partial bytes.
            write_raw(&records_dir.join("torn.json"), half).await?;
        }
        CrashPoint::PersistFsyncParent => {
            // Crash after rename, before the parent-dir fsync → the dir entry
            // may not be durable; model the record reading as absent (write
            // nothing). load_all must still return the sibling.
        }
        CrashPoint::SpoolChunks
        | CrashPoint::SpoolChunkMissing
        | CrashPoint::SpoolMarker
        | CrashPoint::SpoolDir => {
            return Err(format!(
                "persist_boundary_recovers: {cp:?} is not a persist boundary"
            ));
        }
    }

    let recovered: Vec<SimRec> = durable_record::load_all(records_dir, "sim-rec").await;
    let ok = recovered.iter().any(|r| r.id == "survivor");
    let torn_surfaced = recovered.iter().any(|r| r.id == "torn");

    // Clean up before judging, so a later injection starts fresh regardless.
    for name in ["survivor.json", "torn.json", "torn.json.partial"] {
        let _ = tokio::fs::remove_file(records_dir.join(name)).await;
    }

    if !ok {
        return Err(format!(
            "persist boundary {cp:?}: the valid sibling was lost on recovery"
        ));
    }
    if torn_surfaced {
        return Err(format!(
            "persist boundary {cp:?}: a torn record surfaced from load_all"
        ));
    }
    Ok(())
}

async fn write_raw(path: &Path, bytes: &[u8]) -> Result<(), String> {
    tokio::fs::write(path, bytes)
        .await
        .map_err(|e| format!("write {}: {e}", path.display()))
}

fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

/// The lowest-indexed `chunk-<idx>.bin` in `dir` (deterministic; never
/// `read_dir` order).
fn lowest_chunk(dir: &Path) -> Result<Option<(usize, std::path::PathBuf)>, String> {
    let mut best: Option<(usize, std::path::PathBuf)> = None;
    let rd = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in rd {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(idx) = name
            .strip_prefix("chunk-")
            .and_then(|s| s.strip_suffix(".bin"))
            .and_then(|s| s.parse::<usize>().ok())
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| idx < *b) {
            best = Some((idx, entry.path()));
        }
    }
    Ok(best)
}

fn truncate_lowest_chunk(dir: &Path) -> Result<(), String> {
    let Some((_idx, path)) = lowest_chunk(dir)? else {
        return Err(format!("no chunk file to truncate in {}", dir.display()));
    };
    let bytes = std::fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    // Truncate to half length (a strict prefix → digest mismatch / torn).
    std::fs::write(&path, &bytes[..bytes.len() / 2])
        .map_err(|e| format!("truncate {}: {e}", path.display()))
}

fn remove_lowest_chunk(dir: &Path) -> Result<(), String> {
    let Some((_idx, path)) = lowest_chunk(dir)? else {
        return Err(format!("no chunk file to remove in {}", dir.display()));
    };
    remove_file(&path)
}
