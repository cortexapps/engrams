//! One-file-per-record durable JSON persistence — the shared engine
//! under `checkpoint::CheckpointRecord` (ADR 0028 Fix A) and
//! `capture_job::CaptureJobRecord` (ADR 0084 §A).
//!
//! The contract both rely on:
//!   - **persist** = write to a `.json.partial` temp, fsync, rename —
//!     the rename publishes complete bytes or nothing;
//!   - **load_all** = torn-write-tolerant: unreadable/unparseable files
//!     are skipped with a warn, never an error — a torn write must not
//!     wedge the heartbeat loop that re-advertises these records;
//!   - **delete** = idempotent (NotFound is fine): the coordinator acked,
//!     the PG row owns the reference now.
//!
//! Records are keyed by their id's `Display` form: `<dir>/<id>.json`.

use std::fmt::Display;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

pub fn record_path(dir: &Path, id: impl Display) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// Durably persist (write + fsync via rename) `record` at
/// `<dir>/<id>.json`. `what` names the record type in error text.
pub async fn persist<T: Serialize>(
    dir: &Path,
    id: impl Display,
    record: &T,
    what: &str,
) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    let dest = record_path(dir, id);
    let tmp = dest.with_extension("json.partial");
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| std::io::Error::other(format!("serialize {what}: {e}")))?;
    tokio::fs::write(&tmp, &bytes).await?;
    // fsync the temp file so the rename publishes complete bytes.
    let f = tokio::fs::OpenOptions::new().read(true).open(&tmp).await?;
    f.sync_all().await?;
    tokio::fs::rename(&tmp, &dest).await?;
    Ok(())
}

/// All records in `dir`. Unreadable/partial files are skipped with a
/// warn (see the module doc's torn-write contract).
pub async fn load_all<T: DeserializeOwned>(dir: &Path, what: &str) -> Vec<T> {
    let mut out = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return out;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match tokio::fs::read(&p).await {
            Ok(bytes) => match serde_json::from_slice::<T>(&bytes) {
                Ok(r) => out.push(r),
                Err(e) => {
                    tracing::warn!(path = %p.display(), error = %e,
                        "unparseable {what}; skipping");
                }
            },
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e,
                    "unreadable {what}; skipping");
            }
        }
    }
    out
}

/// Delete the acked records' files; NotFound is fine (already gone).
pub async fn delete_acked(dir: &Path, acked: impl IntoIterator<Item = impl Display>, what: &str) {
    for id in acked {
        let p = record_path(dir, id);
        if let Err(e) = tokio::fs::remove_file(&p).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %p.display(), error = %e,
                    "failed to delete acked {what}");
            }
        }
    }
}
