//! Shutdown spool for un-uploaded dirty disk chunks.
//!
//! ADR 0110 status: acked writes now land in a per-sandbox dirty FILE
//! that survives process death, and `rehydrate_sandbox` seeds from that
//! file when it exists. The spool remains only as the recovery seed for
//! a sandbox whose predecessor ran a pre-0110 host-agent (no dirty
//! file on disk). It retires per the ADR 0110 rollout plan.
//!
//! 2026-07-16 session-85e0298a corruption RCA (the world this was built
//! for): NBD WRITEs were acked to the guest from the backend's in-RAM
//! dirty tier; durability rode the FlushScheduler's ~30 s cadence plus
//! a bounded SIGTERM final-flush pass. When that pass overran its
//! budget, the dirty tier died with the process and the successor
//! rehydrated from the last *published* manifest — silently rolling a
//! live guest's disk back by hundreds of MiB of ACKED writes (ext4
//! discovered it minutes later as corrupt bitmaps / `Structure needs
//! cleaning`; 9 such overruns fleet-wide in the preceding week).
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
//!
//! The chunk digests protect the chunk BYTES — but until R5 the marker
//! ITSELF (`meta.json`: the lineage version + the digest list) was
//! unchecksummed JSON, so a bit-flip that stayed valid (a bumped version,
//! an altered digest) was TRUSTED. R5 (ADR 0098 Phase 3) seals `meta.json`
//! in a [`durable_envelope`](crate::durable_envelope) keyed on the
//! `sandbox_id`: [`read_spool`] verifies the marker's content hash + identity
//! before trusting it, so a lying marker is a loud `InvalidData` rollback like
//! any other validation failure.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use engram_chunk_store::ChunkHash;
use engram_core::types::manifest::ManifestRef;
use engram_core::SandboxId;
use engram_host_core::HostFs;
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
    fs: &dyn HostFs,
    root: &Path,
    sandbox_id: SandboxId,
    manifest_ref: ManifestRef,
    chunks: &[(usize, Vec<u8>)],
) -> io::Result<u64> {
    let dir = spool_dir(root, sandbox_id);
    // Replace-don't-merge: a stale prior spool mixed with a fresh one
    // would splice chunks from two divergence points.
    match fs.remove_dir(&dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    fs.create_dir(&dir).await?;

    let mut bytes_total = 0u64;
    let mut spooled = Vec::with_capacity(chunks.len());
    for (idx, data) in chunks {
        let path = dir.join(format!("chunk-{idx}.bin"));
        fs.write(&path, data).await?;
        fs.sync_file(&path).await?;
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
    // R5: seal the completeness marker under a content hash + the sandbox id,
    // so a lying disk cannot bump the version / rewrite a digest undetected.
    let meta_body = serde_json::to_string(&meta)?;
    let meta_bytes = crate::durable_envelope::seal(&sandbox_id.to_string(), &meta_body);
    fs.write(&meta_path, &meta_bytes).await?;
    fs.sync_file(&meta_path).await?;

    // fsync the directory entries (chunk files + meta) and the root's
    // entry for the new directory.
    fs.sync_dir(&dir).await?;
    fs.sync_dir(root).await?;
    Ok(bytes_total)
}

/// Read and verify ONLY the spool's sealed completeness marker for
/// `sandbox_id` — no chunk bytes are touched. `Ok(None)` when there is
/// no complete spool; `Err(InvalidData)` when a marker exists but fails
/// the envelope (torn/bit-rotted/misdirected).
///
/// This is the local survivor-rehydrate pass's disk-lineage source
/// (2026-07-21 61a03b7e incident): the marker's `manifest_ref` is the
/// predecessor's actual chunked-DISK lineage, where the checkpoint
/// `ChainHeadRecord` the pass previously reached for carries the MEMORY
/// chain head — attaching that quarantined the survivor's device on
/// `ManifestKind` mismatch and destroyed a healthy paused VM.
pub async fn read_spool_meta(
    fs: &dyn HostFs,
    root: &Path,
    sandbox_id: SandboxId,
) -> io::Result<Option<SpoolMeta>> {
    let dir = spool_dir(root, sandbox_id);
    let meta_bytes = match fs.read(&dir.join("meta.json")).await {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // R5: open the sealed marker (content hash + sandbox-id identity) BEFORE
    // trusting it. A bit-flipped/misdirected marker is a loud InvalidData
    // rollback — never a trusted lineage/digest.
    let meta_body = crate::durable_envelope::open(&meta_bytes, &sandbox_id.to_string())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("spool meta: {e}")))?;
    let meta: SpoolMeta = serde_json::from_slice(&meta_body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("spool meta body: {e}")))?;
    Ok(Some(meta))
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
    fs: &dyn HostFs,
    root: &Path,
    sandbox_id: SandboxId,
) -> io::Result<Option<(SpoolMeta, Vec<(usize, Vec<u8>)>)>> {
    let dir = spool_dir(root, sandbox_id);
    let Some(meta) = read_spool_meta(fs, root, sandbox_id).await? else {
        return Ok(None);
    };

    // Enumerate the on-disk chunk files by index. Foreign / unparsable
    // entries are IGNORED (a stray tmpfile must not wedge adoption of
    // valid siblings); the marker, not the directory listing, is the
    // authority on which chunks the spool holds.
    let mut on_disk: HashMap<usize, PathBuf> = HashMap::new();
    for path in fs.read_dir(&dir).await? {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(idx) = name
            .strip_prefix("chunk-")
            .and_then(|s| s.strip_suffix(".bin"))
            .and_then(|s| s.parse::<usize>().ok())
        else {
            continue;
        };
        on_disk.insert(idx, path);
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
        let data = fs.read(&path).await?;
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
pub async fn discard_spool(fs: &dyn HostFs, root: &Path, sandbox_id: SandboxId) -> io::Result<()> {
    match fs.remove_dir(&spool_dir(root, sandbox_id)).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_host_core::TokioFs;

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
        let bytes = write_spool(&TokioFs, tmp.path(), sid, refv(41), &chunks)
            .await
            .unwrap();
        assert_eq!(bytes, 80);
        let (meta, back) = read_spool(&TokioFs, tmp.path(), sid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.manifest_ref(), refv(41));
        assert_eq!(back, chunks);
    }

    #[tokio::test]
    async fn absent_and_incomplete_spools_read_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        assert!(read_spool(&TokioFs, tmp.path(), sid)
            .await
            .unwrap()
            .is_none());
        // Chunk file but no meta.json = crash mid-write → absent.
        let dir = tmp.path().join(sid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("chunk-0.bin"), b"x").unwrap();
        assert!(read_spool(&TokioFs, tmp.path(), sid)
            .await
            .unwrap()
            .is_none());
    }

    /// The local survivor-rehydrate pass's lineage probe (61a03b7e):
    /// meta-only read returns the DISK manifest ref without touching
    /// chunk bytes, reads absent as None, and refuses a torn marker.
    #[tokio::test]
    async fn meta_only_read_yields_lineage_and_rejects_torn_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        assert!(read_spool_meta(&TokioFs, tmp.path(), sid)
            .await
            .unwrap()
            .is_none());
        write_spool(&TokioFs, tmp.path(), sid, refv(16), &[(0, vec![1u8; 8])])
            .await
            .unwrap();
        let meta = read_spool_meta(&TokioFs, tmp.path(), sid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.manifest_ref(), refv(16));
        // The meta-only probe must ALSO be usable when chunk files are
        // gone (it never validates them — read_spool does).
        std::fs::remove_file(tmp.path().join(sid.to_string()).join("chunk-0.bin")).unwrap();
        assert!(read_spool_meta(&TokioFs, tmp.path(), sid)
            .await
            .unwrap()
            .is_some());
        // A torn/bit-rotted marker is a loud InvalidData, never a lineage.
        let meta_path = tmp.path().join(sid.to_string()).join("meta.json");
        let mut bytes = std::fs::read(&meta_path).unwrap();
        let mid = bytes.len() / 2;
        bytes.truncate(mid);
        std::fs::write(&meta_path, &bytes).unwrap();
        assert!(read_spool_meta(&TokioFs, tmp.path(), sid).await.is_err());
    }

    #[tokio::test]
    async fn complete_marker_with_wrong_count_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        write_spool(&TokioFs, tmp.path(), sid, refv(1), &[(0, vec![1u8; 8])])
            .await
            .unwrap();
        std::fs::remove_file(tmp.path().join(sid.to_string()).join("chunk-0.bin")).unwrap();
        assert!(read_spool(&TokioFs, tmp.path(), sid).await.is_err());
    }

    #[tokio::test]
    async fn rewrite_replaces_prior_spool() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        write_spool(
            &TokioFs,
            tmp.path(),
            sid,
            refv(1),
            &[(0, vec![1u8; 8]), (1, vec![2u8; 8])],
        )
        .await
        .unwrap();
        write_spool(&TokioFs, tmp.path(), sid, refv(2), &[(5, vec![3u8; 4])])
            .await
            .unwrap();
        let (meta, back) = read_spool(&TokioFs, tmp.path(), sid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.version, 2);
        assert_eq!(back, vec![(5usize, vec![3u8; 4])]);
        discard_spool(&TokioFs, tmp.path(), sid).await.unwrap();
        assert!(read_spool(&TokioFs, tmp.path(), sid)
            .await
            .unwrap()
            .is_none());
    }

    // ── ADR 0099 H5: the post-crash on-disk state space, exhaustively.
    //
    // A shutdown spool is plain files in a directory, so every partial /
    // corrupt state a crash (or bit-rot, or a lying fsync) can leave is
    // externally constructible — no fault-injection trait needed. Each
    // test below builds one such state on disk and asserts read_spool's
    // SAFE behavior: return the FULL verified set, or fail/skip loudly —
    // NEVER hand back a torn or subset view the caller would adopt as
    // acked data. (The caller's Err/None arms discard + roll back; the
    // sin the spool exists to prevent is a *silent* regression.)

    fn chunk_path(root: &Path, sid: SandboxId, idx: usize) -> PathBuf {
        spool_dir(root, sid).join(format!("chunk-{idx}.bin"))
    }

    /// State 1: a chunk file torn/short under a COMPLETE marker. The
    /// write-ordering (chunk fsync before the marker) forbids this from
    /// an orderly crash, but storage corruption can produce it — and
    /// adopting the short bytes would serve a corrupt disk to the guest.
    /// Sweep every truncation offset over a small chunk (durable_record's
    /// exhaustive-truncation approach) plus an in-place bit-flip that
    /// keeps the length: read_spool must reject every one, and must never
    /// silently adopt the valid sibling as a subset.
    #[tokio::test]
    async fn torn_chunk_under_valid_marker_is_rejected_at_every_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        // Chunk 0 is the one we corrupt; chunk 1 is the valid sibling.
        let full: Vec<u8> = (0..24u8).collect();
        let sibling = vec![0xEEu8; 8];
        let good = vec![(0usize, full.clone()), (1usize, sibling.clone())];

        // The pristine spool reads back whole (the sweep's control).
        write_spool(&TokioFs, tmp.path(), sid, refv(7), &good)
            .await
            .unwrap();
        let (_m, back) = read_spool(&TokioFs, tmp.path(), sid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(back, good, "pristine spool must round-trip");

        // Truncate chunk 0 to every length shorter than full → digest
        // mismatch → Err. A zero-length truncation is included.
        for trunc in 0..full.len() {
            std::fs::write(chunk_path(tmp.path(), sid, 0), &full[..trunc]).unwrap();
            let err = read_spool(&TokioFs, tmp.path(), sid)
                .await
                .expect_err("torn chunk must never read back as a complete spool");
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }

        // A same-length mangle (one flipped byte) must also fail — the
        // marker pins content, not just length.
        let mut flipped = full.clone();
        flipped[0] ^= 0xFF;
        std::fs::write(chunk_path(tmp.path(), sid, 0), &flipped).unwrap();
        assert!(read_spool(&TokioFs, tmp.path(), sid).await.is_err());

        // Restoring the exact bytes makes the spool whole again — the
        // rejection was about content, not a wedged directory.
        std::fs::write(chunk_path(tmp.path(), sid, 0), &full).unwrap();
        let (_m, back) = read_spool(&TokioFs, tmp.path(), sid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(back, good, "restoring the bytes re-validates the spool");
    }

    /// State 2: crash mid-export leaves chunk files but no marker. The
    /// marker is written + fsync'd LAST, so its absence means "not
    /// complete" → read as absent (None); the successor serves the last
    /// published manifest and the incomplete spool is inert until
    /// discard_spool sweeps it.
    #[tokio::test]
    async fn missing_completeness_marker_reads_as_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        write_spool(
            &TokioFs,
            tmp.path(),
            sid,
            refv(3),
            &[(0, vec![1u8; 16]), (1, vec![2u8; 16])],
        )
        .await
        .unwrap();
        // Crash between the chunk writes and the marker fsync.
        std::fs::remove_file(spool_dir(tmp.path(), sid).join("meta.json")).unwrap();
        assert!(
            read_spool(&TokioFs, tmp.path(), sid)
                .await
                .unwrap()
                .is_none(),
            "no marker = incomplete spool = absent, never a partial adopt",
        );
        // And it is cleanable (the successor's discard path).
        discard_spool(&TokioFs, tmp.path(), sid).await.unwrap();
        assert!(read_spool(&TokioFs, tmp.path(), sid)
            .await
            .unwrap()
            .is_none());
    }

    /// State 3: a complete marker that lists a chunk whose file is gone.
    /// The valid sibling must NOT be adopted as a subset — a spool is
    /// all-or-nothing, so this is a loud Err, not a silent partial view.
    #[tokio::test]
    async fn marker_lists_a_chunk_whose_file_is_missing_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        write_spool(
            &TokioFs,
            tmp.path(),
            sid,
            refv(9),
            &[(0, vec![1u8; 8]), (4, vec![2u8; 8])],
        )
        .await
        .unwrap();
        std::fs::remove_file(chunk_path(tmp.path(), sid, 4)).unwrap();
        let err = read_spool(&TokioFs, tmp.path(), sid)
            .await
            .expect_err("a marker referencing a missing chunk file must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// State 4: foreign / garbage files beside the valid chunks. Stray
    /// tmpfiles or unparsable names must be tolerated (the marker, not
    /// the directory listing, defines the spool) — the valid siblings
    /// still adopt. But an EXTRA well-named chunk the marker does NOT
    /// list is a splice from another divergence point → Err.
    #[tokio::test]
    async fn garbage_files_tolerated_but_unlisted_extra_chunk_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        let good = vec![(0usize, vec![1u8; 8]), (2usize, vec![2u8; 8])];
        write_spool(&TokioFs, tmp.path(), sid, refv(5), &good)
            .await
            .unwrap();
        let dir = spool_dir(tmp.path(), sid);
        // Foreign siblings: an unparsable name, a bad-index name, a
        // leftover .partial tmpfile, a hidden file.
        std::fs::write(dir.join("README"), b"operator scratch").unwrap();
        std::fs::write(dir.join("chunk-.bin"), b"no index").unwrap();
        std::fs::write(dir.join("chunk-abc.bin"), b"not a number").unwrap();
        std::fs::write(dir.join("chunk-0.bin.partial"), b"torn tmp").unwrap();
        std::fs::write(dir.join(".nfs0001"), b"stale nfs handle").unwrap();
        let (_m, back) = read_spool(&TokioFs, tmp.path(), sid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(back, good, "garbage siblings must not block valid chunks");

        // Now an unlisted but well-named extra chunk → reject (splice).
        std::fs::write(dir.join("chunk-9.bin"), vec![9u8; 8]).unwrap();
        let err = read_spool(&TokioFs, tmp.path(), sid)
            .await
            .expect_err("an unlisted chunk-*.bin is an unaccounted splice");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// State 5: corrupt lineage metadata — the marker file itself is
    /// unparsable (torn mid-write, or garbage). read_spool must reject
    /// it (InvalidData), never guess a lineage. This is the corrupted-
    /// ref-file extension of the lineage-mismatch case whose semantic
    /// half lives at the call site.
    #[tokio::test]
    async fn unparsable_marker_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        write_spool(&TokioFs, tmp.path(), sid, refv(2), &[(0, vec![1u8; 8])])
            .await
            .unwrap();
        let meta = spool_dir(tmp.path(), sid).join("meta.json");
        // Truncated JSON (crash mid-marker-write).
        std::fs::write(&meta, b"{\"manifest_id\":\"abcd").unwrap();
        assert_eq!(
            read_spool(&TokioFs, tmp.path(), sid)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData,
        );
        // Outright garbage.
        std::fs::write(&meta, b"\x00\xff not json at all").unwrap();
        assert!(read_spool(&TokioFs, tmp.path(), sid).await.is_err());
    }

    /// State 6: a zero-chunk, ref-only spool — the "flush+manifest
    /// uploaded but the coord publish was lost" class. It carries the
    /// store-ahead ManifestRef and adopts as an empty chunk set. Extend
    /// with the torn-ref variant: a zero-chunk spool whose marker is
    /// truncated must fail, not be treated as a valid empty spool.
    #[tokio::test]
    async fn zero_chunk_ref_only_spool_roundtrips_and_torn_ref_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        let bytes = write_spool(&TokioFs, tmp.path(), sid, refv(88), &[])
            .await
            .unwrap();
        assert_eq!(bytes, 0);
        let (meta, back) = read_spool(&TokioFs, tmp.path(), sid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.manifest_ref(), refv(88), "store-ahead ref preserved");
        assert!(back.is_empty(), "zero-chunk spool adopts an empty set");

        // Torn ref file on a zero-chunk spool → Err, not a phantom empty.
        std::fs::write(spool_dir(tmp.path(), sid).join("meta.json"), b"{").unwrap();
        assert!(read_spool(&TokioFs, tmp.path(), sid).await.is_err());
    }

    /// State 7 (R5, storage lies): a marker corruption that stays
    /// SYNTACTICALLY VALID. The chunk digests protect the chunk bytes, but the
    /// marker's own lineage/digest fields are the checksum gap: a lying disk
    /// that bumps the recorded `version` (defeating the rebuild's stale-spool
    /// lineage gate) or rewrites a stored digest leaves the marker a perfectly
    /// valid `SpoolMeta`. Pre-envelope that is TRUSTED; the R5 content-hash
    /// envelope makes it a loud `InvalidData` rollback.
    #[tokio::test]
    async fn valid_but_bit_rotted_marker_is_rejected_not_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        let sid = SandboxId::new();
        write_spool(&TokioFs, tmp.path(), sid, refv(2), &[(0, vec![1u8; 8])])
            .await
            .unwrap();
        let meta_path = spool_dir(tmp.path(), sid).join("meta.json");

        // The lie: bump the marker's recorded version to 999 — still a valid
        // SpoolMeta, but the sealed content hash no longer matches. Mutate the
        // envelope's `body` field in place (the hash stays stale).
        let mut env: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        let body = env["body"].as_str().unwrap();
        let mut meta: SpoolMeta = serde_json::from_str(body).unwrap();
        meta.version = 999;
        env["body"] = serde_json::Value::String(serde_json::to_string(&meta).unwrap());
        std::fs::write(&meta_path, serde_json::to_vec(&env).unwrap()).unwrap();

        let err = read_spool(&TokioFs, tmp.path(), sid)
            .await
            .expect_err("a bit-rotted-but-valid marker must be rejected, never trusted");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("checksum"),
            "the rejection reason must name the content-hash gap: {err}",
        );

        // A MISDIRECTED marker — another sandbox's validly-sealed marker served
        // here — is caught on the identity binding, not the hash.
        let other = SandboxId::new();
        write_spool(&TokioFs, tmp.path(), other, refv(2), &[(0, vec![1u8; 8])])
            .await
            .unwrap();
        let foreign = std::fs::read(spool_dir(tmp.path(), other).join("meta.json")).unwrap();
        std::fs::write(&meta_path, &foreign).unwrap();
        let err = read_spool(&TokioFs, tmp.path(), sid)
            .await
            .expect_err("another sandbox's marker at this path is a misdirected read");
        assert!(
            err.to_string().contains("identity"),
            "the rejection reason must name the identity binding: {err}",
        );
    }
}
