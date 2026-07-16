//! ADR 0035: host-side bundle generation store.
//!
//! Bundle generations are content-addressed files
//! (`<drive_id>-<sha256>.squashfs`) staged under the fleet-canonical
//! bundle dir. The FC-host image bakes the *current* generation; every
//! other generation a snapshot pins arrives here by being
//! **materialized** from BlobStorage (`bundles/sha256/<sha>`), and gets
//! there in the first place by being **published** at snapshot time —
//! so blob storage only ever holds generations some snapshot
//! referenced, which is exactly the GC's pin universe.
//!
//! Publish runs on *every* snapshot with aux refs (idempotent
//! HEAD-then-put): base captures publish the enable-time generation,
//! and eviction snapshots publish whatever the VM actually has attached
//! — which after a fresh-create swap (ADR 0035 §3) can be a baked
//! generation no capture ever published.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use engram_core::traits::storage::BlobStorage;
use engram_core::types::sandbox::{AuxBundleRef, AuxRoDrive};
use engram_core::SandboxError;
use futures::StreamExt;
use sha2::{Digest, Sha256};

/// Resolve the staged-bundle dir: `ENGRAM_BUNDLE_DIR` env override
/// (dev / tests), else the fleet-canonical [`AuxRoDrive::SHARED_DIR`].
pub fn bundle_dir_from_env() -> PathBuf {
    std::env::var("ENGRAM_BUNDLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(AuxRoDrive::SHARED_DIR))
}

/// Read the bake-time `current.json` stamp as bundle refs. Missing or
/// malformed stamp → empty + warn — a dev host without staged bundles
/// must still heartbeat (it just reports no current bundles, and any
/// capture requesting aux drives fails loudly at the FC layer).
pub async fn read_stamp(dir: &Path) -> Vec<AuxBundleRef> {
    let path = dir.join(AuxRoDrive::CURRENT_STAMP);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                stamp = %path.display(),
                error = %e,
                "no bundle stamp; reporting no current bundles",
            );
            return Vec::new();
        }
    };
    match serde_json::from_slice::<std::collections::HashMap<String, String>>(&bytes) {
        Ok(map) => {
            let mut refs: Vec<AuxBundleRef> = map
                .into_iter()
                .map(|(drive_id, sha256)| AuxBundleRef { drive_id, sha256 })
                .collect();
            refs.sort_by(|a, b| a.drive_id.cmp(&b.drive_id));
            refs
        }
        Err(e) => {
            tracing::warn!(stamp = %path.display(), error = %e, "malformed bundle stamp");
            Vec::new()
        }
    }
}

/// ADR 0035 §5: heartbeat-ack-driven bundle supervisor. Watches the
/// coord's `live_bundles` pin set and (a) prefetches any pinned
/// generation this host is missing — so resumes land warm instead of
/// paying the materialize on the restore path — and (b) deletes staged
/// generations that are neither pinned nor in the bake stamp ("the
/// bundle path is pinned by GC", host edition). Mirrors the
/// `image_prefetch` supervisor's watch-channel shape.
pub fn spawn_supervisor(
    store: BundleStore,
    current: Vec<AuxBundleRef>,
) -> tokio::sync::watch::Sender<Vec<AuxBundleRef>> {
    /// After a failed materialize, retry on this cadence instead of
    /// waiting for the next `live_bundles` CHANGE — the pin set is
    /// near-static, so "retry on next ack change" was a wedge: a
    /// transient staging failure (the 2026-07-15 fresh-NVMe bringup's
    /// mount race mid-write, a GCS blip) left the host bundle-less
    /// with a healthy agent until a pod restart. Success returns the
    /// loop to pure change-driven waits — no steady-state polling.
    const FAILED_RETRY: std::time::Duration = std::time::Duration::from_secs(60);
    let (tx, mut rx) = tokio::sync::watch::channel(Vec::<AuxBundleRef>::new());
    tokio::spawn(async move {
        let mut failed = false;
        loop {
            if failed {
                tokio::select! {
                    changed = rx.changed() => {
                        if changed.is_err() {
                            tracing::debug!(
                                "bundle supervisor: live_bundles sender dropped; exiting"
                            );
                            return;
                        }
                    }
                    _ = tokio::time::sleep(FAILED_RETRY) => {}
                }
            } else if rx.changed().await.is_err() {
                tracing::debug!("bundle supervisor: live_bundles sender dropped; exiting");
                return;
            }
            let live = rx.borrow_and_update().clone();
            failed = match store.materialize_if_missing(&live).await {
                Ok(()) => false,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        retry_secs = FAILED_RETRY.as_secs(),
                        "bundle prefetch failed; will retry",
                    );
                    true
                }
            };
            store.sweep_unpinned(&live, &current).await;
        }
    });
    tx
}

/// Publish + materialize operations over the staged-bundle dir and
/// BlobStorage. Held by `PooledBackend` when the host has a blob
/// backend wired (production always; tests opt in).
pub struct BundleStore {
    blob: Arc<dyn BlobStorage>,
    dir: PathBuf,
    /// The staged-bundle file extension for THIS host's backend
    /// (`SandboxBackend::bundle_file_ext`): `squashfs` on both backends (ADR 0096).
    /// The staged filename (`<sha>.<ext>`) must match what the backend attaches
    /// (`staged_bundle_path` on VZ), or `materialize_if_missing` misses the
    /// locally-staged file and faults to BlobStorage.
    ext: String,
}

impl BundleStore {
    pub fn new(blob: Arc<dyn BlobStorage>, dir: PathBuf, ext: impl Into<String>) -> Self {
        Self {
            blob,
            dir,
            ext: ext.into(),
        }
    }

    fn staged_file_name(&self, sha256: &str) -> String {
        format!("{sha256}.{}", self.ext)
    }

    pub fn staged_path(&self, r: &AuxBundleRef) -> PathBuf {
        self.dir.join(self.staged_file_name(&r.sha256))
    }

    /// Idempotently publish each referenced generation's bytes to
    /// BlobStorage. Errors are load-bearing: a snapshot whose pinned
    /// bundle isn't durable in blob storage can't be restored on
    /// another host, so the snapshot pipeline must fail (and retry)
    /// rather than record a pin nothing can satisfy.
    pub async fn publish(&self, refs: &[AuxBundleRef]) -> Result<(), SandboxError> {
        for r in refs {
            let key = AuxRoDrive::blob_key(&r.sha256);
            let exists =
                self.blob.exists(&key).await.map_err(|e| {
                    SandboxError::Snapshot(format!("bundle publish: HEAD {key}: {e}"))
                })?;
            if exists {
                continue;
            }
            let staged = self.staged_path(r);
            let file = tokio::fs::File::open(&staged).await.map_err(|e| {
                SandboxError::Snapshot(format!(
                    "bundle publish: open staged {}: {e}",
                    staged.display()
                ))
            })?;
            let stream = engram_core::traits::storage::ByteStream::new(
                tokio_util::io::ReaderStream::new(file)
                    .map(|r| r.map_err(engram_core::error::BlobError::Io)),
            );
            let size =
                self.blob.put_streaming(&key, stream).await.map_err(|e| {
                    SandboxError::Snapshot(format!("bundle publish: put {key}: {e}"))
                })?;
            tracing::info!(
                drive_id = %r.drive_id,
                sha256 = %r.sha256,
                size_bytes = size,
                "bundle generation published to BlobStorage (first reference)",
            );
        }
        Ok(())
    }

    /// Ensure every pinned generation is staged locally, fetching from
    /// BlobStorage (with digest verification + atomic rename) when
    /// missing. Called before `inner.restore*` so FC's `load_snapshot`
    /// finds the embedded path, and by the heartbeat-driven prefetch.
    pub async fn materialize_if_missing(&self, refs: &[AuxBundleRef]) -> Result<(), SandboxError> {
        for r in refs {
            let staged = self.staged_path(r);
            if tokio::fs::try_exists(&staged).await.unwrap_or(false) {
                continue;
            }
            self.fetch_one(r, &staged).await?;
        }
        Ok(())
    }

    /// Delete staged generations that are neither in the coord's pin
    /// set nor in the bake stamp. Best-effort (a failed unlink is just
    /// disk not reclaimed); deleting a file an FC VM still has open is
    /// safe (unlinked-but-open) — the pin set covers every snapshot, so
    /// nothing that needs *re-opening* is ever swept.
    pub async fn sweep_unpinned(&self, live: &[AuxBundleRef], current: &[AuxBundleRef]) {
        let keep: std::collections::HashSet<String> = live
            .iter()
            .chain(current.iter())
            .map(|r| self.staged_file_name(&r.sha256))
            .collect();
        let suffix = format!(".{}", self.ext);
        let mut dir = match tokio::fs::read_dir(&self.dir).await {
            Ok(d) => d,
            Err(_) => return, // no staging dir — nothing to sweep
        };
        while let Ok(Some(entry)) = dir.next_entry().await {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Only files matching the staged-generation shape for this backend's
            // pack format (<sha>.squashfs on both backends, ADR 0096); the stamp
            // itself and any temp files stay.
            if !name.ends_with(&suffix) || keep.contains(name) {
                continue;
            }
            match tokio::fs::remove_file(entry.path()).await {
                Ok(()) => {
                    tracing::info!(file = %name, "swept unpinned bundle generation");
                }
                Err(e) => {
                    tracing::warn!(file = %name, error = %e, "bundle sweep: unlink failed");
                }
            }
        }
    }

    async fn fetch_one(&self, r: &AuxBundleRef, staged: &Path) -> Result<(), SandboxError> {
        let key = AuxRoDrive::blob_key(&r.sha256);
        let started = std::time::Instant::now();
        let mut stream = self.blob.get_streaming(&key).await.map_err(|e| {
            SandboxError::Snapshot(format!(
                "bundle materialize: get {key}: {e} — the pinned generation is \
                 neither staged on this host nor in BlobStorage (GC bug or a \
                 snapshot recorded without publish)"
            ))
        })?;
        if let Some(parent) = staged.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("bundle materialize: mkdir: {e}")))?;
        }
        // Same-dir temp file so the final rename is atomic on the same
        // filesystem; suffix by pid to keep concurrent restores of the
        // same generation from clobbering each other mid-write (last
        // rename wins; both bodies are identical by construction).
        let tmp = staged.with_extension(format!("tmp.{}", std::process::id()));
        let mut file = tokio::fs::File::create(&tmp).await.map_err(|e| {
            SandboxError::Snapshot(format!("bundle materialize: create {}: {e}", tmp.display()))
        })?;
        let mut hasher = Sha256::new();
        let mut size: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| {
                SandboxError::Snapshot(format!("bundle materialize: read {key}: {e}"))
            })?;
            hasher.update(&chunk);
            size += chunk.len() as u64;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("bundle materialize: write: {e}")))?;
        }
        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("bundle materialize: flush: {e}")))?;
        drop(file);
        let got = format!("{:x}", hasher.finalize());
        if got != r.sha256 {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(SandboxError::Snapshot(format!(
                "bundle materialize: digest mismatch for {key}: got {got}, \
                 want {} — refusing to stage corrupt bytes",
                r.sha256
            )));
        }
        tokio::fs::rename(&tmp, staged).await.map_err(|e| {
            SandboxError::Snapshot(format!(
                "bundle materialize: rename {} -> {}: {e}",
                tmp.display(),
                staged.display()
            ))
        })?;
        tracing::info!(
            drive_id = %r.drive_id,
            sha256 = %r.sha256,
            size_bytes = size,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "bundle generation materialized from BlobStorage",
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_storage_local::LocalBlobStorage;

    fn sha_of(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn store(tmp: &tempfile::TempDir) -> BundleStore {
        BundleStore::new(
            Arc::new(LocalBlobStorage::new(tmp.path().join("blob"))),
            tmp.path().join("shared"),
            "squashfs",
        )
    }

    fn aux(drive_id: &str, body: &[u8]) -> AuxBundleRef {
        AuxBundleRef {
            drive_id: drive_id.into(),
            sha256: sha_of(body),
        }
    }

    async fn stage(s: &BundleStore, r: &AuxBundleRef, body: &[u8]) {
        let p = s.staged_path(r);
        tokio::fs::create_dir_all(p.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&p, body).await.unwrap();
    }

    #[tokio::test]
    async fn publish_uploads_once_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp);
        let body = b"skills-generation-1";
        let r = aux("skills", body);
        stage(&s, &r, body).await;

        s.publish(std::slice::from_ref(&r)).await.unwrap();
        let key = AuxRoDrive::blob_key(&r.sha256);
        assert_eq!(s.blob.get(&key).await.unwrap().as_ref(), body);

        // Second publish: HEAD-hit, no error (and doesn't need the
        // staged file re-read — but proving "no rewrite" cheaply:
        // delete the staged file; an idempotent publish still passes).
        tokio::fs::remove_file(s.staged_path(&r)).await.unwrap();
        s.publish(std::slice::from_ref(&r)).await.unwrap();
    }

    #[tokio::test]
    async fn publish_fails_loud_when_staged_file_missing() {
        // A pin nothing can satisfy must fail the snapshot pipeline,
        // not record silently (the restore would fail much later,
        // on another host, with less context).
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp);
        let r = aux("skills", b"never-staged");
        let err = s.publish(std::slice::from_ref(&r)).await.unwrap_err();
        assert!(format!("{err}").contains("open staged"), "{err}");
    }

    #[tokio::test]
    async fn materialize_fetches_verifies_and_renames() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp);
        let body = b"playwright-generation-7";
        let r = aux("browser", body);
        s.blob
            .put(
                &AuxRoDrive::blob_key(&r.sha256),
                bytes::Bytes::from_static(body),
            )
            .await
            .unwrap();

        s.materialize_if_missing(std::slice::from_ref(&r))
            .await
            .unwrap();
        let staged = s.staged_path(&r);
        assert_eq!(tokio::fs::read(&staged).await.unwrap(), body);
        // Idempotent: present file short-circuits (no blob hit needed).
        s.materialize_if_missing(std::slice::from_ref(&r))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn materialize_rejects_digest_mismatch() {
        // Corrupt blob bytes (or a ref/key mixup) must never land in
        // the staging dir — a wrong-content staged file is exactly the
        // incident's failure mode.
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp);
        let r = AuxBundleRef {
            drive_id: "skills".into(),
            sha256: sha_of(b"expected-bytes"),
        };
        s.blob
            .put(
                &AuxRoDrive::blob_key(&r.sha256),
                bytes::Bytes::from_static(b"DIFFERENT-bytes"),
            )
            .await
            .unwrap();
        let err = s
            .materialize_if_missing(std::slice::from_ref(&r))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("digest mismatch"), "{err}");
        assert!(!s.staged_path(&r).exists());
        // The temp file was cleaned up too — staging dir holds nothing.
        let mut entries = std::fs::read_dir(tmp.path().join("shared"))
            .map(|d| d.count())
            .unwrap_or(0);
        // (dir exists because fetch_one mkdir'd it)
        let _ = &mut entries;
        assert_eq!(entries, 0);
    }

    #[tokio::test]
    async fn materialize_fails_loud_on_blob_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp);
        let r = aux("skills", b"unpublished");
        let err = s
            .materialize_if_missing(std::slice::from_ref(&r))
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("neither staged on this host nor in BlobStorage"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn sweep_keeps_pinned_and_current_deletes_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(&tmp);
        let pinned = aux("skills", b"old-but-pinned");
        let current = aux("skills", b"the-baked-current");
        let garbage = aux("skills", b"unreferenced");
        for (r, body) in [
            (&pinned, b"old-but-pinned" as &[u8]),
            (&current, b"the-baked-current"),
            (&garbage, b"unreferenced"),
        ] {
            stage(&s, r, body).await;
        }
        // The stamp file must survive the sweep.
        tokio::fs::write(
            tmp.path().join("shared").join(AuxRoDrive::CURRENT_STAMP),
            b"{}",
        )
        .await
        .unwrap();

        s.sweep_unpinned(
            std::slice::from_ref(&pinned),
            std::slice::from_ref(&current),
        )
        .await;

        assert!(s.staged_path(&pinned).exists());
        assert!(s.staged_path(&current).exists());
        assert!(!s.staged_path(&garbage).exists());
        assert!(tmp
            .path()
            .join("shared")
            .join(AuxRoDrive::CURRENT_STAMP)
            .exists());
    }

    #[tokio::test]
    async fn read_stamp_missing_and_malformed_degrade_to_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_stamp(tmp.path()).await.is_empty());
        tokio::fs::write(tmp.path().join(AuxRoDrive::CURRENT_STAMP), b"not-json")
            .await
            .unwrap();
        assert!(read_stamp(tmp.path()).await.is_empty());
        tokio::fs::write(
            tmp.path().join(AuxRoDrive::CURRENT_STAMP),
            br#"{"skills": "abc", "browser": "def"}"#,
        )
        .await
        .unwrap();
        let refs = read_stamp(tmp.path()).await;
        assert_eq!(refs.len(), 2);
        // Sorted by drive_id for deterministic heartbeats.
        assert_eq!(refs[0].drive_id, "browser");
        assert_eq!(refs[1].drive_id, "skills");
    }

    /// A failed materialize self-heals on the retry timer WITHOUT an
    /// ack change (prod 2026-07-15: the near-static pin set meant
    /// "retry on next ack change" never fired and the host sat
    /// bundle-less until a pod restart). `start_paused` auto-advances
    /// the 60 s retry sleep, so the test runs in milliseconds.
    #[tokio::test(start_paused = true)]
    async fn failed_materialize_retries_on_timer_not_just_ack_change() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let blob_dir = tempfile::tempdir().unwrap();
        let staged_dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(blob_dir.path().to_path_buf()),
        );
        let store = BundleStore::new(blob.clone(), staged_dir.path().to_path_buf(), "squashfs");
        // Content-addressed: the pin sha must be the real digest of the
        // bytes the blob will hold (materialize verifies on stage).
        let body = bytes::Bytes::from_static(b"squashfs bytes");
        let sha = {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(&body);
            format!("{:x}", h.finalize())
        };
        let staged = staged_dir.path().join(format!("{sha}.squashfs"));
        let pin = vec![AuxBundleRef {
            drive_id: "skills".into(),
            sha256: sha.clone(),
        }];

        let tx = spawn_supervisor(store, Vec::new());
        // First attempt: the blob doesn't exist yet — materialize fails.
        tx.send(pin.clone()).unwrap();
        tokio::task::yield_now().await;
        assert!(!tokio::fs::try_exists(&staged).await.unwrap());

        // Publish the blob; do NOT touch the watch channel. The retry
        // timer alone must stage it.
        blob.put(&AuxRoDrive::blob_key(&sha), body).await.unwrap();
        for _ in 0..200 {
            if tokio::fs::try_exists(&staged).await.unwrap() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        panic!("retry timer never staged the bundle (still wedged on ack change)");
    }
}
