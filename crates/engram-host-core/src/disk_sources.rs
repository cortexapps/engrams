//! Sources: read-only block devices that each serve one disk manifest,
//! for a cloned drive to read the regions it has not copied (ADR 0127).
//! A role has at most one source, from the lazy restore to the cutover.
//!
//! A drive's I/O waits on its source, and the kernel cannot remove a
//! device that has a request nothing will finish. So a source is stopped
//! only after every drive and view over it is gone, and a source that died
//! is recovered before any drive over it is suspended or removed.

use std::io;
use std::path::PathBuf;

use async_trait::async_trait;
use engram_chunk_store::ChunkStore;
use engram_core::types::ids::SandboxId;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::snapshot::DiskRole;
use uuid::Uuid;

use crate::disk::{FileDisk, SessionDisk, Volume, VolumeName};
use crate::disk_flows::stage;

#[async_trait]
pub trait DiskSources: Send + Sync {
    /// Start a source of `role` that serves `manifest`, and return its
    /// device path. `base`, a staged volume and the manifest it holds,
    /// serves the chunks it shares with `manifest`. `AlreadyExists` when
    /// the role has a source.
    async fn start(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        manifest: ManifestRef,
        base: Option<(&Volume, ManifestRef)>,
    ) -> io::Result<PathBuf>;

    /// Make the role's source serve I/O, starting it again on the same
    /// device when it died. `NotFound` when the role has no source.
    async fn ensure_alive(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()>;

    /// Stop the role's source. No drive or view may read it. A role with
    /// no source is left as it is.
    async fn stop(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()>;

    /// Every role that has a source.
    async fn list(&self) -> io::Result<Vec<(SandboxId, DiskRole)>>;
}

/// Sources as files the manifest is staged into, one per role in one
/// directory: for [`FileDisk`], whose drives copy their source in full
/// before they return. A file has no process that can die.
pub struct FileSources {
    root: PathBuf,
    store: ChunkStore,
}

impl FileSources {
    pub fn new(root: impl Into<PathBuf>, store: ChunkStore) -> io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root, store })
    }

    fn name(sandbox: SandboxId, role: DiskRole) -> VolumeName {
        VolumeName::new(format!("{}-{role}", sandbox.as_uuid().simple()))
            .expect("a source name is 37 bytes of [a-z0-9-]")
    }

    fn path(&self, sandbox: SandboxId, role: DiskRole) -> PathBuf {
        self.root.join(Self::name(sandbox, role).as_str())
    }
}

#[async_trait]
impl DiskSources for FileSources {
    /// Reads every chunk from the store; `base` saves nothing here.
    async fn start(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        manifest: ManifestRef,
        _base: Option<(&Volume, ManifestRef)>,
    ) -> io::Result<PathBuf> {
        let manifest = self
            .store
            .get_manifest(manifest)
            .await
            .map_err(io::Error::other)?;
        let files = FileDisk::new(&self.root, manifest.chunk_size.as_u64())?;
        let name = Self::name(sandbox, role);
        match stage(&files, &self.store, &name, &manifest, None).await {
            Ok(volume) => Ok(volume.path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
            Err(e) => {
                let _ = files.delete(&name).await;
                Err(e)
            }
        }
    }

    async fn ensure_alive(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()> {
        std::fs::metadata(self.path(sandbox, role)).map(drop)
    }

    async fn stop(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()> {
        match std::fs::remove_file(self.path(sandbox, role)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    async fn list(&self) -> io::Result<Vec<(SandboxId, DiskRole)>> {
        let mut roles = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let name = entry?.file_name();
            let Some((sandbox, role)) = name.to_str().and_then(|n| n.split_once('-')) else {
                continue;
            };
            if let (Ok(sandbox), Ok(role)) = (Uuid::try_parse(sandbox), role.parse()) {
                roles.push((SandboxId::from(sandbox), role));
            }
        }
        Ok(roles)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use engram_chunk_store::{ChunkSize, Manifest, ManifestKind};
    use engram_storage_local::LocalBlobStorage;

    use super::*;

    #[tokio::test]
    async fn a_file_source_holds_the_manifest_until_it_stops() {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(Arc::new(LocalBlobStorage::new(dir.path().join("blobs"))));
        let manifest = ManifestRef::new();
        let empty = Manifest {
            chunk_size: ChunkSize::bytes(4096),
            ..Manifest::empty(ManifestKind::Disk, 8192)
        };
        store.put_manifest(manifest, &empty).await.unwrap();
        let sources = FileSources::new(dir.path().join("sources"), store).unwrap();
        let (sandbox, role) = (SandboxId::new(), DiskRole::Swap);

        let path = sources.start(sandbox, role, manifest, None).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), vec![0; 8192]);
        let again = sources.start(sandbox, role, manifest, None).await;
        assert_eq!(again.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert!(path.exists(), "a refused start keeps the running source");
        sources.ensure_alive(sandbox, role).await.unwrap();
        assert_eq!(sources.list().await.unwrap(), vec![(sandbox, role)]);

        sources.stop(sandbox, role).await.unwrap();
        sources.stop(sandbox, role).await.unwrap();
        assert!(sources.list().await.unwrap().is_empty());
        let gone = sources.ensure_alive(sandbox, role).await.unwrap_err();
        assert_eq!(gone.kind(), io::ErrorKind::NotFound);
    }
}
