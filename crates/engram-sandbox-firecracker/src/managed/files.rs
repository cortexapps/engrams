use std::{io, path::Path};

fn verify_file(path: &Path, digest: &str, size: Option<u64>) -> io::Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || size.is_some_and(|n| n != metadata.len()) {
        return Err(io::Error::other("invalid managed file size or type"));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    if hex::encode(hash.finalize()) != digest {
        return Err(io::Error::other("managed artifact digest mismatch"));
    }
    Ok(())
}

pub(super) struct PinnedFiles {
    pub path: std::path::PathBuf,
}
impl PinnedFiles {
    pub fn prepare(
        work: &Path,
        cfg: &crate::FirecrackerConfig,
        request: &engram_core::types::managed_cold::ManagedColdPrepare,
        spec: &engram_core::types::sandbox::SandboxSpec,
    ) -> io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::create_dir_all(work)?;
        use sha2::{Digest, Sha256};
        let name = hex::encode(Sha256::digest(
            format!("{}\0{}", request.sandbox_id, request.boot_attempt).as_bytes(),
        ));
        // Keep the control-socket path short. This digest is only a filename;
        // exclusive creation rejects a collision, never adopts another owner.
        let path = work.join(format!("m-{}", &name[..24]));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        let files = Self { path };
        files.pin(
            spec.rootfs_source
                .as_deref()
                .ok_or_else(|| io::Error::other("missing selected disk"))?,
            "rootfs.ext4",
            &request.disk.sha256,
            Some(request.disk.size_bytes),
        )?;
        files.pin(
            &cfg.kernel_image_path,
            "kernel",
            &request.kernel_sha256,
            None,
        )?;
        for drive in &request.aux_ro_drives {
            let hash = drive
                .sha256
                .as_deref()
                .ok_or_else(|| io::Error::other("unresolved drive"))?;
            let name = format!("{hash}.squashfs");
            if !files.path.join(&name).exists() {
                files.pin(&cfg.bundle_dir.join(&name), &name, hash, None)?;
            }
        }
        if let Some(mib) = spec.swap_mib.filter(|n| *n > 0) {
            std::fs::File::create_new(files.path.join("swap.img"))?
                .set_len(u64::from(mib) * 1024 * 1024)?;
        }
        Ok(files)
    }
    fn pin(&self, source: &Path, name: &str, hash: &str, size: Option<u64>) -> io::Result<()> {
        // File input only. A pooled block device must be materialized at the
        // exact selected version before entering this cold constructor.
        if !std::fs::metadata(source)?.is_file() {
            return Err(io::Error::other("managed input is not a file"));
        }
        let destination = self.path.join(name);
        std::fs::copy(source, &destination)?;
        verify_file(&destination, hash, size)
    }
}
impl Drop for PinnedFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    pub(in crate::managed) fn inputs(
        dir: &Path,
    ) -> (
        crate::FirecrackerConfig,
        engram_core::types::managed_cold::ManagedColdPrepare,
        engram_core::types::sandbox::SandboxSpec,
    ) {
        use engram_core::types::{
            managed_cold::{ManagedColdDisk, ManagedColdPrepare, ManagedColdSource},
            manifest::ManifestRef,
            sandbox::AuxRoDrive,
        };
        let hash = |bytes: &[u8]| hex::encode(Sha256::digest(bytes));
        std::fs::write(dir.join("kernel"), b"kernel-a").unwrap();
        std::fs::write(dir.join("disk"), b"disk-a").unwrap();
        let bundle = hash(b"bundle");
        std::fs::write(dir.join(format!("{bundle}.squashfs")), b"bundle").unwrap();
        let drives: Vec<_> = (0..3)
            .map(|i| AuxRoDrive {
                drive_id: AuxRoDrive::slot_drive_id(i),
                guest_mount: AuxRoDrive::slot_guest_mount(i),
                fs_type: "squashfs".into(),
                sha256: Some(bundle.clone()),
            })
            .collect();
        let manifest = ManifestRef::new();
        let spec: engram_core::types::sandbox::SandboxSpec = serde_json::from_value(serde_json::json!({"image":"image-v1", "rootfs_source":dir.join("disk"), "rootfs_manifest":manifest,
            "cpu":{"vcpus":1}, "memory":{"max_mib":128}, "disk":{"max_gib":1}, "env":{}, "aux_ro_drives":drives})).unwrap();
        let request = ManagedColdPrepare {
            version: 1,
            session_id: engram_core::SessionId::new(),
            sandbox_id: engram_core::SandboxId::new(),
            boot_attempt: "boot-1".into(),
            image: spec.image.clone(),
            disk: ManagedColdDisk {
                manifest,
                sha256: hash(b"disk-a"),
                size_bytes: 6,
                source: ManagedColdSource::FreshImage {
                    base_manifest: ManifestRef::new(),
                },
            },
            kernel_sha256: hash(b"kernel-a"),
            aux_ro_drives: spec.aux_ro_drives.clone(),
        };
        let mut cfg = crate::FirecrackerConfig::with_kernel(dir.join("kernel"));
        cfg.bundle_dir = dir.to_owned();
        cfg.balloon = false;
        (cfg, request, spec)
    }
    #[test]
    fn uuid_boot_attempt_fits_native_control_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, mut request, spec) = inputs(dir.path());
        request.boot_attempt = "12345678-1234-1234-1234-123456789abc".into();
        let files = PinnedFiles::prepare(&dir.path().join("owned"), &cfg, &request, &spec).unwrap();
        let runtime_path = Path::new("/var/lib/engrams")
            .join(files.path.file_name().unwrap())
            .join("firecracker.sock");
        assert!(
            std::os::unix::net::SocketAddr::from_pathname(runtime_path).is_ok(),
            "ordinary UUID boot attempts must fit a Unix control socket"
        );
    }
    #[test]
    fn pinning_is_private_and_collision_does_not_remove_previous_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, request, spec) = inputs(dir.path());
        let work = dir.path().join("owned");
        let files = PinnedFiles::prepare(&work, &cfg, &request, &spec).unwrap();
        let owned = files.path.clone();
        std::fs::write(dir.path().join("disk"), b"disk-b").unwrap();
        assert_eq!(std::fs::read(owned.join("rootfs.ext4")).unwrap(), b"disk-a");
        assert!(PinnedFiles::prepare(&work, &cfg, &request, &spec).is_err());
        assert!(owned.exists());
        drop(files);
        assert!(!owned.exists());
        assert!(PinnedFiles::prepare(&work, &cfg, &request, &spec).is_err());
        assert!(!owned.exists());
    }
    #[test]
    fn selected_bytes_and_length_must_match_after_copy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk");
        std::fs::write(&path, b"disk-a").unwrap();
        let digest = hex::encode(Sha256::digest(b"disk-a"));
        verify_file(&path, &digest, Some(6)).unwrap();
        std::fs::write(&path, b"disk-b").unwrap();
        assert!(verify_file(&path, &digest, Some(6)).is_err());
        std::fs::write(&path, b"disk-a").unwrap();
        assert!(verify_file(&path, &digest, Some(7)).is_err());
    }
}
