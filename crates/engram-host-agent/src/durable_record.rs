//! One-file-per-record durable JSON persistence — the shared engine
//! under `checkpoint::CheckpointRecord` (ADR 0028 Fix A) and
//! `capture_job::CaptureJobRecord` (ADR 0084 §A).
//!
//! The contract both rely on:
//!   - **persist** = write to a `.json.partial` temp, fsync, rename, then
//!     fsync the parent dir — the rename publishes complete bytes or
//!     nothing, and the dir fsync makes the rename itself crash-durable;
//!   - **load_all** = torn-write-tolerant: unreadable/unparseable files
//!     are skipped with a warn, never an error — a torn write must not
//!     wedge the heartbeat loop that re-advertises these records;
//!   - **delete** = idempotent (NotFound is fine): the coordinator acked,
//!     the PG row owns the reference now.
//!
//! Records are keyed by their id's `Display` form: `<dir>/<id>.json`.
//!
//! Every durable op goes through the injected [`HostFs`] (ADR 0098 P5) so
//! the host-internal simulator's `CrashFs` can crash BETWEEN operations;
//! flows not yet behind the seam pass [`TokioFs`] at their wrappers.
//!
//! **Content envelope (ADR 0098 Phase 3, R5 — storage lies).** The record
//! bytes are wrapped in a [`durable_envelope`](crate::durable_envelope): a
//! sha256 over the body + the record's id (its filename stem). `load_all`
//! verifies both, so a bit-flip / lying fsync that leaves the JSON valid is
//! a LOUD skip (a DISTINCT `checksum mismatch` log), not a silently-trusted
//! parse — and a misdirected read (another id's sealed bytes at this path)
//! is caught too. This is a clean-break format bump; these files are
//! host-local transient state with no cross-host/version reader.

use std::fmt::Display;
use std::path::{Path, PathBuf};

use engram_host_core::HostFs;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub fn record_path(dir: &Path, id: impl Display) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// Durably persist (write + fsync via rename) `record` at
/// `<dir>/<id>.json`. `what` names the record type in error text.
pub async fn persist<T: Serialize>(
    fs: &dyn HostFs,
    dir: &Path,
    id: impl Display,
    record: &T,
    what: &str,
) -> std::io::Result<()> {
    fs.create_dir(dir).await?;
    let id = id.to_string();
    let dest = record_path(dir, &id);
    let tmp = dest.with_extension("json.partial");
    let body = serde_json::to_string_pretty(record)
        .map_err(|e| std::io::Error::other(format!("serialize {what}: {e}")))?;
    // R5: seal the body under a content hash + the record id (the filename
    // stem), so a lying disk cannot hand a resume a corrupt-but-valid record.
    let bytes = crate::durable_envelope::seal(&id, &body);
    fs.write(&tmp, &bytes).await?;
    // fsync the temp file so the rename publishes complete bytes.
    fs.sync_file(&tmp).await?;
    fs.rename(&tmp, &dest).await?;
    // fsync the PARENT DIRECTORY so the rename itself is durable — without
    // this, a crash after `rename` returns can still lose the directory
    // entry (the file's data is synced, but the dir's updated block that
    // points at it may sit only in the page cache). Opening a directory
    // read-only for `fsync` is valid on Linux and macOS.
    fs.sync_dir(dir).await?;
    Ok(())
}

/// All records in `dir`. Unreadable/partial files are skipped with a
/// warn (see the module doc's torn-write contract).
pub async fn load_all<T: DeserializeOwned>(fs: &dyn HostFs, dir: &Path, what: &str) -> Vec<T> {
    let mut out = Vec::new();
    let Ok(entries) = fs.read_dir(dir).await else {
        return out;
    };
    for p in entries {
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // The record's expected identity is its filename stem — bind the
        // envelope's sealed id to it so a misdirected read is rejected.
        let expected_id = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        match fs.read(&p).await {
            Ok(bytes) => match crate::durable_envelope::open(&bytes, expected_id) {
                Ok(body) => match serde_json::from_slice::<T>(&body) {
                    Ok(r) => out.push(r),
                    Err(e) => {
                        tracing::warn!(path = %p.display(), error = %e,
                            "unparseable {what} body; skipping");
                    }
                },
                // R5: DISTINCT logs — a checksum/identity failure is
                // corruption (a real finding), not a torn write.
                Err(e @ crate::durable_envelope::OpenError::ChecksumMismatch { .. }) => {
                    tracing::warn!(path = %p.display(), error = %e,
                        "corrupt {what} (checksum); skipping");
                }
                Err(e @ crate::durable_envelope::OpenError::IdMismatch { .. }) => {
                    tracing::warn!(path = %p.display(), error = %e,
                        "misdirected {what} (identity); skipping");
                }
                Err(e @ crate::durable_envelope::OpenError::Malformed(_)) => {
                    tracing::warn!(path = %p.display(), error = %e,
                        "malformed {what} envelope; skipping");
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
pub async fn delete_acked(
    fs: &dyn HostFs,
    dir: &Path,
    acked: impl IntoIterator<Item = impl Display>,
    what: &str,
) {
    for id in acked {
        let p = record_path(dir, id);
        if let Err(e) = fs.remove_file(&p).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %p.display(), error = %e,
                    "failed to delete acked {what}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! ADR 0099 H5: storage fault injection for the torn-write contract,
    //! *no new trait and no `fail` crate*. Every post-crash on-disk state a
    //! partial `persist` can leave — a truncated `.json` (rename published
    //! nothing yet), a `.json` with a garbage tail, a leftover `.json.partial`
    //! (crash before the rename) — is externally constructible precisely
    //! because [`load_all`] takes a plain directory. So we build those states
    //! directly and assert the module doc's promise: unreadable/unparseable
    //! files are skipped, valid sibling records still load, nothing panics.
    //!
    //! Out of scope here: a crash *during* a run — a kill wedged BETWEEN the
    //! temp fsync and the rename, or mid-write. That is byte-level in-flight
    //! fault injection, which ADR 0098 phase 2's `SimFs` owns; `persist`'s
    //! rename-or-nothing publish means the only states it can leave on disk
    //! are the ones enumerated above, and this module covers all of them.

    use super::*;
    use engram_host_core::TokioFs;

    #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct Rec {
        id: String,
        payload: String,
    }

    fn rec(id: &str, payload_len: usize) -> Rec {
        Rec {
            id: id.to_string(),
            payload: "x".repeat(payload_len),
        }
    }

    async fn write_raw(dir: &Path, name: &str, bytes: &[u8]) {
        tokio::fs::create_dir_all(dir).await.unwrap();
        tokio::fs::write(dir.join(name), bytes).await.unwrap();
    }

    /// The real `persist` path round-trips through `load_all` unchanged —
    /// anchors the contract the torn-state sweeps then perturb.
    #[tokio::test]
    async fn persist_then_load_all_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let a = rec("alpha", 64);
        let b = rec("beta", 128);
        persist(&TokioFs, dir, &a.id, &a, "rec").await.unwrap();
        persist(&TokioFs, dir, &b.id, &b, "rec").await.unwrap();

        let mut out: Vec<Rec> = load_all(&TokioFs, dir, "rec").await;
        out.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(out, vec![a, b]);
    }

    /// Exhaustive truncation sweep: for EVERY byte offset of a ~1 KB record,
    /// a `.json` truncated at that offset sitting beside two valid sibling
    /// records must leave `load_all` returning *exactly* the two siblings —
    /// never the torn record, never an error, never a panic. Cheap and total
    /// at this size, so a literal loop beats proptest (ADR 0099 H5).
    #[tokio::test]
    async fn truncation_at_every_offset_tolerated_siblings_survive() {
        let a = rec("sibling-a", 128);
        let b = rec("sibling-b", 256);

        // ~1 KB record whose every truncation prefix we sweep — as the REAL
        // on-disk shape, an R5 envelope (seal over the body + id).
        let torn = rec("torn", 1000);
        let torn_body = serde_json::to_string_pretty(&torn).unwrap();
        let full = crate::durable_envelope::seal("torn", &torn_body);
        assert!(
            full.len() >= 1000,
            "sealed record should be ~1 KB, got {}",
            full.len()
        );

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Siblings via the real persist path (enveloped) so load_all accepts them.
        persist(&TokioFs, dir, &a.id, &a, "rec").await.unwrap();
        persist(&TokioFs, dir, &b.id, &b, "rec").await.unwrap();

        for offset in 0..full.len() {
            // Overwrite the torn file at each truncation length. offset 0 is
            // the empty-file case (rename published nothing); every offset <
            // len is a strict prefix, so the closing `}` is never present and
            // the value can never accidentally parse as a whole record.
            write_raw(dir, "torn.json", &full[..offset]).await;

            let mut out: Vec<Rec> = load_all(&TokioFs, dir, "rec").await;
            out.sort_by(|x, y| x.id.cmp(&y.id));
            assert_eq!(
                out,
                vec![a.clone(), b.clone()],
                "offset {offset}: load_all must return exactly the valid siblings, \
                 never the torn record, and never error/panic",
            );
        }
    }

    /// Garbage-suffix and leftover-`.json.partial` variants of the same
    /// tolerance: valid JSON + appended junk is skipped (serde rejects the
    /// trailing bytes), and a `.json.partial` (crash before rename) is
    /// ignored by the extension filter even when it holds valid bytes.
    #[tokio::test]
    async fn garbage_suffix_and_leftover_partial_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let a = rec("sibling-a", 128);
        let b = rec("sibling-b", 256);
        persist(&TokioFs, dir, &a.id, &a, "rec").await.unwrap();
        persist(&TokioFs, dir, &b.id, &b, "rec").await.unwrap();

        // Valid JSON body with a garbage tail appended.
        let mut poisoned = serde_json::to_vec_pretty(&rec("poison", 64)).unwrap();
        poisoned.extend_from_slice(b"\x00\xff not json trailing garbage");
        write_raw(dir, "poison.json", &poisoned).await;

        // A leftover `.json.partial` (crashed before the rename). Even
        // holding otherwise-valid bytes it must be skipped — its extension is
        // not `.json`, so the rename never "published" it.
        let partial = serde_json::to_vec_pretty(&rec("leftover", 64)).unwrap();
        write_raw(dir, "leftover.json.partial", &partial).await;

        let mut out: Vec<Rec> = load_all(&TokioFs, dir, "rec").await;
        out.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(
            out,
            vec![a, b],
            "garbage-suffix and .json.partial must both skip; the two siblings still load",
        );
    }

    /// R5 (storage lies): the checksum-gap the envelope closes. A
    /// syntactically-VALID corruption — a body byte-flipped into a different
    /// valid record — would be TRUSTED by a pre-envelope loader; `load_all`
    /// now rejects it (checksum mismatch) and drops it. And a MISDIRECTED read
    /// — another id's validly-sealed bytes served at this path — is rejected on
    /// the identity binding. In both cases the valid sibling still loads.
    #[tokio::test]
    async fn checksum_and_identity_corruption_are_rejected_not_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let good = rec("good", 64);
        persist(&TokioFs, dir, &good.id, &good, "rec")
            .await
            .unwrap();

        // (1) Bit-rot that stays valid JSON: mutate the sealed body in place
        // WITHOUT updating the content hash. Pre-envelope this parses fine.
        {
            let victim = rec("victim", 64);
            persist(&TokioFs, dir, &victim.id, &victim, "rec")
                .await
                .unwrap();
            let path = record_path(dir, &victim.id);
            let mut env: serde_json::Value =
                serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
            // A different, still-valid Rec body — the stored hash no longer matches.
            let mangled = serde_json::to_string(&rec("victim", 4096)).unwrap();
            env["body"] = serde_json::Value::String(mangled);
            write_raw(dir, "victim.json", &serde_json::to_vec(&env).unwrap()).await;
        }

        // (2) Misdirection: another id's sealed bytes dropped at foreign.json.
        {
            let elsewhere = rec("elsewhere", 32);
            let body = serde_json::to_string_pretty(&elsewhere).unwrap();
            let sealed = crate::durable_envelope::seal("elsewhere", &body);
            write_raw(dir, "foreign.json", &sealed).await;
        }

        let out: Vec<Rec> = load_all(&TokioFs, dir, "rec").await;
        assert_eq!(
            out,
            vec![good],
            "a checksum-failing body and a misdirected read are both dropped; \
             only the untouched sibling survives",
        );
    }
}
