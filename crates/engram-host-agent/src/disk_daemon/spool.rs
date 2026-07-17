//! Shutdown spool for un-uploaded dirty disk chunks.
//!
//! 2026-07-16 session-85e0298a corruption RCA: NBD WRITEs are acked to
//! the guest from the backend's in-RAM dirty tier; durability rides the
//! FlushScheduler's ~30 s cadence plus a bounded SIGTERM final-flush
//! pass. When that pass overran its budget, the dirty tier died with
//! the process and the successor rehydrated from the last *published*
//! manifest — silently rolling a live guest's disk back by hundreds of
//! MiB of ACKED writes (ext4 discovered it minutes later as corrupt
//! bitmaps / `Structure needs cleaning`; 9 such overruns fleet-wide in
//! the preceding week).
//!
//! The spool closes that window: at shutdown, after the data planes are
//! abandoned (serve loops dead, so the tier is frozen and later guest
//! writes park in the kernel's dead-conn window for the successor to
//! replay), every remaining un-uploaded chunk is written to
//! `<spool_root>/<sandbox_id>/` on the hostPath volume — local NVMe,
//! sub-second, no GCS round-trip racing `terminationGracePeriodSeconds`.
//! The successor's `rehydrate_sandbox` adopts the spool back into the
//! fresh backend's dirty tier (and the scheduler uploads it promptly),
//! so acked writes survive the roll. A spool is only valid against the
//! manifest lineage it diverged from: `meta.json` records the
//! `ManifestRef`, and adoption rules live at the call site.
//!
//! Layout (`meta.json` is written + fsync'd LAST — its presence marks
//! the spool complete; a crash mid-write leaves no meta and the spool
//! reads as absent):
//!
//! ```text
//! <spool_root>/<sandbox_id>/chunk-<idx>.bin
//! <spool_root>/<sandbox_id>/meta.json
//! ```
//!
//! The marker records the sha256 of every chunk it lists, and
//! [`read_spool`] re-hashes each file before returning it. The
//! write-ordering (chunk fsync BEFORE the marker) means an orderly
//! crash never leaves a torn chunk under a complete marker — but the
//! spool is plain files on a hostPath volume, so bit-rot / a lying
//! fsync / an out-of-band mangle CAN. Adopting a torn chunk as acked
//! data would re-introduce the exact ext4 `Structure needs cleaning`
//! corruption the spool exists to prevent (session-85e0298a), so the
//! digest is a hard gate, not decoration: a mismatch is an `Err` the
//! caller turns into a loud rollback, never a silent adopt.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use engram_chunk_store::ChunkHash;
use engram_core::types::manifest::ManifestRef;
use engram_core::SandboxId;
use serde::{Deserialize, Serialize};

/// One chunk a complete spool holds: its index and the sha256 of its
/// bytes, so [`read_spool`] can prove the file on disk is the bytes
/// [`write_spool`] intended (never a torn/short remnant).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpooledChunk {
    pub idx: usize,
    pub digest: ChunkHash,
}

/// Completeness + lineage marker for one sandbox's spool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpoolMeta {
    /// Manifest lineage the spooled chunks diverge from.
    pub manifest_id: uuid::Uuid,
    /// Manifest version the spooled chunks diverge from.
    pub version: u64,
    /// Every `chunk-*.bin` file a complete spool holds, keyed by index,
    /// each with the sha256 of its bytes. The count AND the content of
    /// the spool are both pinned here — a missing file, an extra file,
    /// or a torn file all fail validation.
    pub chunks: Vec<SpooledChunk>,
}

impl SpoolMeta {
    pub fn manifest_ref(&self) -> ManifestRef {
        ManifestRef {
            manifest_id: self.manifest_id,
            version: self.version,
        }
    }
}

fn spool_dir(root: &Path, sandbox_id: SandboxId) -> PathBuf {
    root.join(sandbox_id.to_string())
}

/// Durably write `chunks` (chunk_idx → full chunk bytes) for
/// `sandbox_id`. Replaces any prior spool for the sandbox. Returns the
/// total payload bytes written. Every file is fsync'd, `meta.json` is
/// written last, and the directory entries are fsync'd, so a spool
/// that reads back complete is durable against the process dying at
/// any point after this returns.
pub async fn write_spool(
    root: &Path,
    sandbox_id: SandboxId,
    manifest_ref: ManifestRef,
    chunks: &[(usize, Vec<u8>)],
) -> io::Result<u64> {
    let dir = spool_dir(root, sandbox_id);
    // Replace-don't-merge: a stale prior spool mixed with a fresh one
    // would splice chunks from two divergence points.
    match tokio::fs::remove_dir_all(&dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    tokio::fs::create_dir_all(&dir).await?;

    let mut bytes_total = 0u64;
    let mut spooled = Vec::with_capacity(chunks.len());
    for (idx, data) in chunks {
        let path = dir.join(format!("chunk-{idx}.bin"));
        let mut f = tokio::fs::File::create(&path).await?;
        tokio::io::AsyncWriteExt::write_all(&mut f, data).await?;
        f.sync_all().await?;
        bytes_total += data.len() as u64;
        spooled.push(SpooledChunk {
            idx: *idx,
            digest: ChunkHash::of(data),
        });
    }

    let meta = SpoolMeta {
        manifest_id: manifest_ref.manifest_id,
        version: manifest_ref.version,
        chunks: spooled,
    };
    let meta_path = dir.join("meta.json");
    let mut f = tokio::fs::File::create(&meta_path).await?;
    tokio::io::AsyncWriteExt::write_all(&mut f, &serde_json::to_vec(&meta)?).await?;
    f.sync_all().await?;

    // fsync the directory entries (chunk files + meta) and the root's
    // entry for the new directory.
    for d in [dir.as_path(), root] {
        let d = d.to_path_buf();
        tokio::task::spawn_blocking(move || std::fs::File::open(&d)?.sync_all())
            .await
            .map_err(|e| io::Error::other(format!("dir fsync task join: {e}")))??;
    }
    Ok(bytes_total)
}

/// Read the spool for `sandbox_id`. `Ok(None)` when there is no
/// complete spool (no dir, or no `meta.json` — e.g. a crash mid-write).
/// `Err` when a spool claims completeness but fails validation — a
/// listed chunk file is missing, a chunk's bytes don't match the sha256
/// the marker recorded (torn/short/mangled), an unparsable chunk name,
/// or extra chunk files the marker doesn't account for. The caller
/// should log and [`discard_spool`], never adopt: every validation
/// failure is a LOUD rollback, never a silent adopt of a subset or of
/// corrupt bytes.
///
/// Foreign, non-`chunk-*.bin` files (stray tmpfiles from another writer,
/// operator scratch) are tolerated — only the marker's own listed
/// chunks define the spool.
pub async fn read_spool(
    root: &Path,
    sandbox_id: SandboxId,
) -> io::Result<Option<(SpoolMeta, Vec<(usize, Vec<u8>)>)>> {
    let dir = spool_dir(root, sandbox_id);
    let meta_bytes = match tokio::fs::read(dir.join("meta.json")).await {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta: SpoolMeta = serde_json::from_slice(&meta_bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("spool meta: {e}")))?;

    // Enumerate the on-disk chunk files by index. Foreign / unparsable
    // entries are IGNORED (a stray tmpfile must not wedge adoption of
    // valid siblings); the marker, not the directory listing, is the
    // authority on which chunks the spool holds.
    let mut on_disk: HashMap<usize, PathBuf> = HashMap::new();
    let mut entries = tokio::fs::read_dir(&dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(idx) = name
            .strip_prefix("chunk-")
            .and_then(|s| s.strip_suffix(".bin"))
            .and_then(|s| s.parse::<usize>().ok())
        else {
            continue;
        };
        on_disk.insert(idx, entry.path());
    }

    // Drive the read from the marker: every chunk it lists must be
    // present AND hash to the recorded digest. This subsumes the count
    // check (a missing file is caught here) and, crucially, rejects a
    // torn chunk under a complete marker — bytes that pass name+count
    // but not content must never be adopted as acked data.
    let mut chunks = Vec::with_capacity(meta.chunks.len());
    for want in &meta.chunks {
        let Some(path) = on_disk.remove(&want.idx) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("spool marker lists chunk {} but no file holds it", want.idx),
            ));
        };
        let data = tokio::fs::read(&path).await?;
        let got = ChunkHash::of(&data);
        if got != want.digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "spool chunk {} digest mismatch (len {}): marker {} != bytes {}",
                    want.idx,
                    data.len(),
                    want.digest.to_hex(),
                    got.to_hex(),
                ),
            ));
        }
        chunks.push((want.idx, data));
    }
    // Any `chunk-*.bin` the marker did NOT list is an extra chunk — a
    // spool spliced from two divergence points would surface here.
    if let Some((extra, _)) = on_disk.into_iter().next() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("spool holds chunk {extra} the marker does not account for"),
        ));
    }
    chunks.sort_by_key(|(idx, _)| *idx);
    Ok(Some((meta, chunks)))
}

/// Remove the spool for `sandbox_id`, if any.
pub async fn discard_spool(root: &Path, sandbox_id: SandboxId) -> io::Result<()> {
    match tokio::fs::remove_dir_all(spool_dir(root, sandbox_id)).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refv(version: u64) -> ManifestRef {
        ManifestRef {
            manifest_id: uuid::Uuid::from_u128(0xabcd),
            version,
        }
    }

    #[tokio::test]
    async fn roundtrip_preserves_chunks_and_lineage() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        let chunks = vec![(3usize, vec![7u8; 64]), (640, vec![9u8; 16])];
        let bytes = write_spool(tmp.path(), sid, refv(41), &chunks)
            .await
            .unwrap();
        assert_eq!(bytes, 80);
        let (meta, back) = read_spool(tmp.path(), sid).await.unwrap().unwrap();
        assert_eq!(meta.manifest_ref(), refv(41));
        assert_eq!(back, chunks);
    }

    #[tokio::test]
    async fn absent_and_incomplete_spools_read_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        assert!(read_spool(tmp.path(), sid).await.unwrap().is_none());
        // Chunk file but no meta.json = crash mid-write → absent.
        let dir = tmp.path().join(sid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("chunk-0.bin"), b"x").unwrap();
        assert!(read_spool(tmp.path(), sid).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn complete_marker_with_wrong_count_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        write_spool(tmp.path(), sid, refv(1), &[(0, vec![1u8; 8])])
            .await
            .unwrap();
        std::fs::remove_file(tmp.path().join(sid.to_string()).join("chunk-0.bin")).unwrap();
        assert!(read_spool(tmp.path(), sid).await.is_err());
    }

    #[tokio::test]
    async fn rewrite_replaces_prior_spool() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        write_spool(
            tmp.path(),
            sid,
            refv(1),
            &[(0, vec![1u8; 8]), (1, vec![2u8; 8])],
        )
        .await
        .unwrap();
        write_spool(tmp.path(), sid, refv(2), &[(5, vec![3u8; 4])])
            .await
            .unwrap();
        let (meta, back) = read_spool(tmp.path(), sid).await.unwrap().unwrap();
        assert_eq!(meta.version, 2);
        assert_eq!(back, vec![(5usize, vec![3u8; 4])]);
        discard_spool(tmp.path(), sid).await.unwrap();
        assert!(read_spool(tmp.path(), sid).await.unwrap().is_none());
    }
}
