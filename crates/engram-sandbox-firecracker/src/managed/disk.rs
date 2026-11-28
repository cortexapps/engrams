//! Session volumes retained by the managed child owner until the VM is reaped.
use std::{io, path::Path, sync::Arc};

use engram_core::{types::snapshot::DiskRole, SandboxId};
use engram_host_core::{
    disk::{ChunkIndex, SessionDisk, Volume, VolumeName},
    session_volumes::{live_name, transient_name},
};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) struct Disks {
    disk: Arc<dyn SessionDisk>,
    sandbox: SandboxId,
    owned: tokio::sync::Mutex<Vec<VolumeName>>,
    runtime: tokio::runtime::Handle,
    retain: std::sync::atomic::AtomicBool,
}
impl Disks {
    pub fn new(disk: Arc<dyn SessionDisk>, sandbox: SandboxId) -> Self {
        Self {
            disk,
            sandbox,
            owned: tokio::sync::Mutex::new(Vec::new()),
            runtime: tokio::runtime::Handle::current(),
            retain: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn retain_source(&self) {
        self.retain
            .store(true, std::sync::atomic::Ordering::Release);
    }

    async fn create(&self, role: DiskRole, bytes: u64) -> io::Result<Volume> {
        use std::os::unix::fs::FileTypeExt;
        let name = live_name(self.sandbox, role);
        let volume = self.disk.create(&name, bytes).await?;
        self.owned.lock().await.push(name);
        if !std::fs::metadata(&volume.path)?
            .file_type()
            .is_block_device()
        {
            return Err(io::Error::other(
                "managed Firecracker requires a block session volume",
            ));
        }
        Ok(volume)
    }

    pub async fn install(
        &self,
        directory: &Path,
        sha256: &str,
        bytes: u64,
        swap_mib: Option<u32>,
    ) -> io::Result<()> {
        let result = async {
            let root = self.create(DiskRole::Root, bytes).await?;
            import(&root, &directory.join("selected.ext4"), sha256).await?;
            std::os::unix::fs::symlink(&root.path, directory.join("rootfs.ext4"))?;
            if let Some(mib) = swap_mib.filter(|n| *n > 0) {
                let swap = self
                    .create(DiskRole::Swap, u64::from(mib) * 1024 * 1024)
                    .await?;
                swap.sync().await?;
                std::os::unix::fs::symlink(&swap.path, directory.join("swap.img"))?;
            }
            std::fs::remove_file(directory.join("selected.ext4"))?;
            Ok(())
        }
        .await;
        if result.is_err() {
            self.release().await?;
        }
        result
    }

    /// The caller has stopped and reaped the VM before taking this disk cut.
    pub async fn export(&self, destination: &Path) -> io::Result<()> {
        self.retain
            .store(true, std::sync::atomic::Ordering::Release);
        let root = self
            .disk
            .open(&live_name(self.sandbox, DiskRole::Root))
            .await?;
        root.sync().await?;
        let name = transient_name(self.sandbox, DiskRole::Root);
        let snapshot = self.disk.snapshot(&root, &name).await?;
        self.owned.lock().await.push(name.clone());
        export(&snapshot, destination).await?;
        drop(snapshot);
        self.disk.delete(&name).await?;
        self.owned.lock().await.retain(|n| n != &name);
        self.retain
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    pub async fn release(&self) -> io::Result<()> {
        if self.retain.load(std::sync::atomic::Ordering::Acquire) {
            return Err(io::Error::other(
                "managed disk retained after incomplete capture",
            ));
        }
        let mut owned = self.owned.lock().await;
        while let Some(name) = owned.last() {
            self.disk.delete(name).await?;
            owned.pop();
        }
        Ok(())
    }
}
impl Drop for Disks {
    fn drop(&mut self) {
        if self.retain.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let names = std::mem::take(self.owned.get_mut());
        let disk = self.disk.clone();
        self.runtime.spawn(async move {
            for name in names.into_iter().rev() {
                if let Err(error) = disk.delete(&name).await {
                    tracing::warn!(volume = %name, %error, "managed disk cleanup incomplete");
                }
            }
        });
    }
}

async fn import(volume: &Volume, source: &Path, expected_hash: &str) -> io::Result<()> {
    let mut source = tokio::fs::File::open(source).await?;
    let metadata = source.metadata().await?;
    if !metadata.is_file() || metadata.len() != volume.size_bytes {
        return Err(io::Error::other("managed disk source size or type changed"));
    }
    let mut hash = Sha256::new();
    for index in (0..volume.chunk_count()).map(ChunkIndex) {
        let mut bytes = vec![0; volume.chunk_len(index)?];
        source.read_exact(&mut bytes).await?;
        hash.update(&bytes);
        if bytes.iter().any(|b| *b != 0) {
            volume.write_chunk(index, bytes).await?;
        }
    }
    if source.read(&mut [0]).await? != 0 || hex::encode(hash.finalize()) != expected_hash {
        return Err(io::Error::other("managed disk source digest changed"));
    }
    volume.sync().await
}

async fn export(volume: &Volume, destination: &Path) -> io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)
        .await?;
    for index in (0..volume.chunk_count()).map(ChunkIndex) {
        file.write_all(&volume.read_chunk(index).await?).await?;
    }
    file.sync_all().await?;
    tokio::fs::File::open(
        destination
            .parent()
            .ok_or_else(|| io::Error::other("capture parent missing"))?,
    )
    .await?
    .sync_all()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_host_core::FileDisk;
    use std::os::unix::fs::PermissionsExt;

    struct BusyDisk {
        inner: FileDisk,
        busy: std::sync::atomic::AtomicBool,
        pause_delete: Option<Arc<tokio::sync::Notify>>,
    }
    #[async_trait::async_trait]
    impl SessionDisk for BusyDisk {
        async fn create(&self, name: &VolumeName, bytes: u64) -> io::Result<Volume> {
            self.inner.create(name, bytes).await
        }
        async fn snapshot(&self, origin: &Volume, name: &VolumeName) -> io::Result<Volume> {
            self.inner.snapshot(origin, name).await
        }
        async fn open(&self, name: &VolumeName) -> io::Result<Volume> {
            self.inner.open(name).await
        }
        async fn delete(&self, name: &VolumeName) -> io::Result<()> {
            if let Some(pause) = &self.pause_delete {
                pause.notified().await;
            }
            if self.busy.load(std::sync::atomic::Ordering::Acquire) {
                Err(io::ErrorKind::ResourceBusy.into())
            } else {
                self.inner.delete(name).await
            }
        }
        async fn list(&self) -> io::Result<Vec<VolumeName>> {
            self.inner.list().await
        }
        async fn rename(&self, from: &VolumeName, to: &VolumeName) -> io::Result<()> {
            self.inner.rename(from, to).await
        }
        async fn capacity(&self) -> io::Result<engram_host_core::Capacity> {
            self.inner.capacity().await
        }
    }

    #[tokio::test]
    async fn started_normal_destroy_releases_disks_but_uncertain_capture_does_not() {
        for (destroy, uncertain, capture_failed, busy, released) in [
            (true, false, false, false, true),
            (false, false, false, false, false),
            (true, true, false, false, false),
            (true, false, true, false, false),
            (true, false, false, true, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let disk = Arc::new(BusyDisk {
                inner: FileDisk::new(dir.path().join("volumes"), 4096).unwrap(),
                busy: std::sync::atomic::AtomicBool::new(busy),
                pause_delete: None,
            });
            let sandbox = SandboxId::new();
            let root = live_name(sandbox, DiskRole::Root);
            disk.create(&root, 4096).await.unwrap();
            let disks = Disks::new(disk.clone(), sandbox);
            disks.owned.lock().await.push(root.clone());
            if capture_failed {
                disks.retain_source();
            }
            let files = super::super::files::PinnedFiles {
                path: dir.path().join("private"),
                retain: std::sync::atomic::AtomicBool::new(true),
                capture_ready: std::sync::atomic::AtomicBool::new(false),
                disks: Some(disks),
            };
            assert_eq!(files.close(destroy, uncertain).await.is_ok(), released);
            drop(files);
            assert_eq!(disk.open(&root).await.is_err(), released);
        }
    }

    #[tokio::test]
    async fn exported_capture_keeps_source_until_host_records_its_hash_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(FileDisk::new(dir.path().join("volumes"), 4096).unwrap());
        let sandbox = SandboxId::new();
        let root = live_name(sandbox, DiskRole::Root);
        disk.create(&root, 4096).await.unwrap();
        let disks = Disks::new(disk.clone(), sandbox);
        disks.owned.lock().await.push(root.clone());
        let files = super::super::files::PinnedFiles {
            path: dir.path().join("private"),
            retain: std::sync::atomic::AtomicBool::new(true),
            capture_ready: std::sync::atomic::AtomicBool::new(false),
            disks: Some(disks),
        };
        assert!(files.capture_stored().is_err());
        files
            .export_capture(&dir.path().join("capture"))
            .await
            .unwrap();
        assert!(
            files.close(false, true).await.is_err(),
            "unrecorded export is not release authority"
        );
        assert!(disk.open(&root).await.is_ok());
        files.capture_stored().unwrap();
        files.close(false, true).await.unwrap();
        assert!(disk.open(&root).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_destroy_preserves_the_started_source() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(BusyDisk {
            inner: FileDisk::new(dir.path().join("volumes"), 4096).unwrap(),
            busy: std::sync::atomic::AtomicBool::new(false),
            pause_delete: Some(Arc::new(tokio::sync::Notify::new())),
        });
        let sandbox = SandboxId::new();
        let root = live_name(sandbox, DiskRole::Root);
        disk.create(&root, 4096).await.unwrap();
        let disks = Disks::new(disk.clone(), sandbox);
        disks.owned.lock().await.push(root.clone());
        let files = super::super::files::PinnedFiles {
            path: dir.path().join("private"),
            retain: std::sync::atomic::AtomicBool::new(true),
            capture_ready: std::sync::atomic::AtomicBool::new(false),
            disks: Some(disks),
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), files.close(true, false))
                .await
                .is_err()
        );
        drop(files);
        assert!(disk.open(&root).await.is_ok());
    }

    #[tokio::test]
    async fn busy_volume_fails_release_without_losing_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(BusyDisk {
            inner: FileDisk::new(dir.path().join("volumes"), 4096).unwrap(),
            busy: std::sync::atomic::AtomicBool::new(true),
            pause_delete: None,
        });
        let sandbox = SandboxId::new();
        let root = live_name(sandbox, DiskRole::Root);
        disk.create(&root, 4096).await.unwrap();
        let owner = Disks::new(disk.clone(), sandbox);
        owner.owned.lock().await.push(root.clone());
        assert_eq!(
            owner.release().await.unwrap_err().kind(),
            io::ErrorKind::ResourceBusy
        );
        assert!(disk.open(&root).await.is_ok());
        disk.busy.store(false, std::sync::atomic::Ordering::Release);
        owner.release().await.unwrap();
        assert!(disk.open(&root).await.is_err());
    }

    #[tokio::test]
    async fn import_and_snapshot_export_preserve_bytes_and_reject_changed_sources() {
        let dir = tempfile::tempdir().unwrap();
        let disk = FileDisk::new(dir.path().join("volumes"), 4096).unwrap();
        let volume = disk
            .create(&VolumeName::new("source").unwrap(), 8192)
            .await
            .unwrap();
        let mut bytes = vec![0; 8192];
        bytes[4096..].fill(23);
        let source = dir.path().join("source");
        std::fs::write(&source, &bytes).unwrap();
        let hash = hex::encode(Sha256::digest(&bytes));
        import(&volume, &source, &hash).await.unwrap();
        let snap = disk
            .snapshot(&volume, &VolumeName::new("cut").unwrap())
            .await
            .unwrap();
        volume
            .write_chunk(ChunkIndex(1), vec![42; 4096])
            .await
            .unwrap();
        let output = dir.path().join("capture");
        export(&snap, &output).await.unwrap();
        assert_eq!(
            std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o600,
            "capture must be private at creation"
        );
        assert_eq!(std::fs::read(&output).unwrap(), bytes);
        assert!(
            export(&snap, &output).await.is_err(),
            "capture must not overwrite evidence"
        );
        bytes[0] = 1;
        std::fs::write(&source, &bytes).unwrap();
        assert!(import(&volume, &source, &hash).await.is_err());
        std::fs::write(&source, b"short").unwrap();
        assert!(import(&volume, &source, &hash).await.is_err());
    }

    #[tokio::test]
    async fn export_releases_its_snapshot_and_close_releases_only_owned_volumes() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(FileDisk::new(dir.path().join("volumes"), 4096).unwrap());
        let sandbox = SandboxId::new();
        let root = live_name(sandbox, DiskRole::Root);
        let foreign = VolumeName::new("foreign").unwrap();
        disk.create(&foreign, 4096).await.unwrap();
        let volume = disk.create(&root, 4096).await.unwrap();
        volume
            .write_chunk(ChunkIndex(0), vec![7; 4096])
            .await
            .unwrap();
        drop(volume);
        let owner = Disks::new(disk.clone(), sandbox);
        owner.owned.lock().await.push(root.clone());
        let output = dir.path().join("capture");
        owner.export(&output).await.unwrap();
        assert_eq!(std::fs::read(output).unwrap(), vec![7; 4096]);
        assert!(disk
            .open(&transient_name(sandbox, DiskRole::Root))
            .await
            .is_err());
        assert!(disk.open(&root).await.is_ok());
        owner.release().await.unwrap();
        assert_eq!(disk.list().await.unwrap(), vec![foreign]);
    }

    #[tokio::test]
    async fn failed_export_keeps_source_and_snapshot_and_refuses_release() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(FileDisk::new(dir.path().join("volumes"), 4096).unwrap());
        let sandbox = SandboxId::new();
        let root = live_name(sandbox, DiskRole::Root);
        disk.create(&root, 4096).await.unwrap();
        let owner = Disks::new(disk.clone(), sandbox);
        owner.owned.lock().await.push(root.clone());
        assert!(owner
            .export(&dir.path().join("missing/capture"))
            .await
            .is_err());
        assert!(owner.release().await.is_err());
        drop(owner);
        assert!(disk.open(&root).await.is_ok());
        assert!(disk
            .open(&transient_name(sandbox, DiskRole::Root))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn file_disk_is_rejected_and_new_volume_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(FileDisk::new(dir.path().join("volumes"), 4096).unwrap());
        let owner = Disks::new(disk.clone(), SandboxId::new());
        assert!(owner
            .install(dir.path(), &"a".repeat(64), 8192, None)
            .await
            .is_err());
        assert!(disk.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn existing_volume_is_never_adopted_or_deleted_on_create_failure() {
        let dir = tempfile::tempdir().unwrap();
        let disk = Arc::new(FileDisk::new(dir.path().join("volumes"), 4096).unwrap());
        let sandbox = SandboxId::new();
        let name = live_name(sandbox, DiskRole::Root);
        let volume = disk.create(&name, 4096).await.unwrap();
        volume
            .write_chunk(ChunkIndex(0), vec![19; 4096])
            .await
            .unwrap();
        let owner = Disks::new(disk.clone(), sandbox);
        assert!(owner
            .install(dir.path(), &"a".repeat(64), 4096, None)
            .await
            .is_err());
        owner.release().await.unwrap();
        assert_eq!(
            disk.open(&name)
                .await
                .unwrap()
                .read_chunk(ChunkIndex(0))
                .await
                .unwrap(),
            vec![19; 4096]
        );
    }
}
