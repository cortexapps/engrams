//! Closed VZ construction and owned helper lifetime. No start permit lives here.
use std::{io, path::Path};

fn verify_file(path: &Path, digest: &str, size: Option<u64>) -> io::Result<()> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || size.is_some_and(|s| s != metadata.len())
        || !engram_core::types::managed_cold::is_sha256(digest)
    {
        return Err(io::Error::other("invalid managed artifact"));
    }
    let mut hash = Sha256::new();
    let mut buf = [0; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    if hex::encode(hash.finalize()) != digest {
        return Err(io::Error::other("managed artifact mismatch"));
    }
    Ok(())
}

/// The selected helper artifact is copied and verified before execution. The
/// digest must come from the trusted installed host artifact set.
#[derive(Clone, Debug)]
pub struct ManagedHelperArtifact {
    pub path: std::path::PathBuf,
    pub sha256: String,
}

/// A stopped VM and its closed network owner. No operation can release it in
/// this producer. It is never inserted into the ordinary sandbox map.
pub struct PreparedManagedVz {
    // Drop the native machine before files or the attachment owner.
    vm: crate::vm::VzVm,
    helper: helper::Helper,
    _files: PinnedFiles,
    receipt: ManagedVzClosedReceipt,
    failed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ManagedVzClosedReceipt {
    pub prepared: engram_core::types::managed_cold::ManagedColdPrepared,
    pub allocation: engram_egress_proto::private::Allocation,
    pub mac: String,
    pub mtu: u32,
    pub helper_sha256: String,
}

impl PreparedManagedVz {
    /// A fresh liveness check, never a final enforcement or start receipt.
    pub async fn closed_receipt(
        &mut self,
    ) -> Result<ManagedVzClosedReceipt, engram_core::SandboxError> {
        let native_closed = tokio::time::timeout(
            std::time::Duration::from_secs(engram_egress_proto::private::HANDOFF_SECONDS),
            self.vm.managed_attachment_closed(),
        )
        .await
        .unwrap_or(false);
        if self.failed || !native_closed || self.helper.check_closed().is_err() {
            self.failed = true;
            let _ = self.helper.close();
            return Err(io::Error::other("managed preparation is no longer live").into());
        }
        Ok(self.receipt.clone())
    }
    /// Wait for owned process cleanup. Success does not acknowledge the proxy
    /// policy barrier. Only that proxy can release its lease or poison its gate.
    pub fn close(mut self) -> Result<(), engram_core::SandboxError> {
        self.failed = true;
        self.helper.close()?;
        Ok(())
    }
}

pub(crate) async fn prepare(
    work_dir: &Path,
    cfg: &crate::backend::VzConfig,
    request: engram_core::types::managed_cold::ManagedColdPrepare,
    spec: engram_core::types::sandbox::SandboxSpec,
    network: engram_egress_proto::private::RegisteredPrivateNetwork,
    artifact: ManagedHelperArtifact,
) -> Result<PreparedManagedVz, engram_core::SandboxError> {
    use engram_core::{
        types::managed_cold::{ManagedColdPrepared, ManagedColdState},
        SandboxError,
    };
    request.validate(&spec).map_err(SandboxError::InvalidSpec)?;
    let allocation = network.allocation().clone();
    if allocation.session_id != request.session_id
        || allocation.sandbox_id != request.sandbox_id
        || allocation.boot_attempt != request.boot_attempt
    {
        return Err(SandboxError::InvalidSpec(
            "managed allocation does not match cold request".into(),
        ));
    }
    let private_stream = network.into_stream();
    let pin = {
        let request = request.clone();
        let spec = spec.clone();
        let cfg = cfg.clone();
        let work_dir = work_dir.to_owned();
        let artifact = artifact.clone();
        tokio::task::spawn_blocking(move || {
            PinnedFiles::prepare(&work_dir, &cfg, &request, &spec, &artifact)
        })
    };
    // Before a helper exists, this stream owns no packet or flow work. Return
    // cleanup EOF at once on proxy stop, even if the disk copy is still running.
    // The blocking task owns its files and removes them when its result drops.
    let files = tokio::select! {
        biased;
        _ = wait_for_private_stop(&private_stream) => {
            drop(private_stream);
            return Err(io::Error::other("private allocation closed during preparation").into());
        }
        result = pin => result.map_err(|_| io::Error::other("artifact preparation failed"))??,
    };
    let (packet, peer) = std::os::unix::net::UnixDatagram::pair()?;
    configure_packet(&packet)?;
    configure_packet(&peer)?;
    // A point-to-point attachment has no shared Ethernet segment. Its fixed
    // unicast source is selected here and shared with the exact helper config.
    let mac = "02:00:00:00:00:02".to_owned();
    let vm_cfg = crate::vm::VmConfig::new(
        files.path.join("kernel"),
        files.path.join("rootfs.ext4"),
        if spec.memory.max_mib > 0 {
            spec.memory.max_mib
        } else {
            cfg.default_memory_mib
        },
        if spec.cpu.vcpus > 0 {
            spec.cpu.vcpus
        } else {
            cfg.default_vcpus
        },
    )
    .with_aux_ro_drives(request.aux_ro_drives.clone(), files.path.clone())
    .with_swap_path(
        spec.swap_mib
            .filter(|m| *m > 0)
            .map(|_| files.path.join("swap.img")),
    )
    .with_machine_identifier(crate::vm::fresh_machine_identifier())
    .with_managed_network(crate::vm::ManagedVmNetwork {
        packet: std::sync::Arc::new(packet),
        guest: allocation.guest_ip,
        gateway: allocation.gateway_ip,
        mac: mac.clone(),
    })?;
    let helper = helper::Helper::spawn(
        &files.path.join("netstack"),
        peer,
        private_stream,
        &helper::Config {
            version: 1,
            guest: allocation.guest_ip,
            gateway: allocation.gateway_ip,
            mac: mac.clone(),
        },
    )
    .await?;
    // New kernel/device construction only. Neither start nor restore is called.
    let vm = crate::vm::VzVm::new(vm_cfg)?;
    let mut prepared = PreparedManagedVz {
        vm,
        helper,
        _files: files,
        failed: false,
        receipt: ManagedVzClosedReceipt {
            prepared: ManagedColdPrepared {
                version: 1,
                request,
                state: ManagedColdState::Closed,
            },
            allocation,
            mac,
            mtu: 1500,
            helper_sha256: artifact.sha256,
        },
    };
    prepared.closed_receipt().await?;
    Ok(prepared)
}

async fn wait_for_private_stop(stream: &tokio::net::UnixStream) {
    loop {
        // No reply is valid here: registration has already consumed CLOSED and
        // the unbound allocation has no operation in flight. Data also fails closed.
        match stream.try_read(&mut [0]) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if stream.readable().await.is_err() {
                    return;
                }
            }
            _ => return,
        }
    }
}

fn configure_packet(socket: &std::os::unix::net::UnixDatagram) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // Apple's attachment requires receive >= 2 * send. Bound both queues.
    for (option, size) in [(libc::SO_SNDBUF, 16_384_i32), (libc::SO_RCVBUF, 65_536_i32)] {
        // SAFETY: socket owns this FD, and the integer pointer and size agree.
        let result = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&size as *const i32).cast(),
                std::mem::size_of_val(&size) as libc::socklen_t,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

struct PinnedFiles {
    path: std::path::PathBuf,
}
impl PinnedFiles {
    fn prepare(
        work_dir: &Path,
        cfg: &crate::backend::VzConfig,
        request: &engram_core::types::managed_cold::ManagedColdPrepare,
        spec: &engram_core::types::sandbox::SandboxSpec,
        artifact: &ManagedHelperArtifact,
    ) -> Result<Self, engram_core::SandboxError> {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::create_dir_all(work_dir)?;
        let path = work_dir.join(format!(
            "managed-{}-{}",
            request.sandbox_id, request.boot_attempt
        ));
        // No overwrite or adoption of an earlier attempt's directory.
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        let files = Self { path };
        files.pin(
            spec.rootfs_source
                .as_deref()
                .ok_or_else(|| io::Error::other("missing disk"))?,
            "rootfs.ext4",
            &request.disk.sha256,
            Some(request.disk.size_bytes),
        )?;
        files.pin(&cfg.kernel_path, "kernel", &request.kernel_sha256, None)?;
        for drive in &request.aux_ro_drives {
            let hash = drive
                .sha256
                .as_deref()
                .ok_or_else(|| io::Error::other("unresolved boot drive"))?;
            let filename = format!("{hash}.squashfs");
            // Several slots can intentionally have the same immutable content.
            if !files.path.join(&filename).exists() {
                files.pin(&cfg.bundle_dir.join(&filename), &filename, hash, None)?;
            }
        }
        files.pin(&artifact.path, "netstack", &artifact.sha256, None)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            files.path.join("netstack"),
            std::fs::Permissions::from_mode(0o500),
        )?;
        if let Some(mib) = spec.swap_mib.filter(|m| *m > 0) {
            let swap = std::fs::File::create_new(files.path.join("swap.img"))?;
            swap.set_len(u64::from(mib) * 1024 * 1024)?;
        }
        Ok(files)
    }
    fn pin(
        &self,
        source: &Path,
        name: &str,
        digest: &str,
        size: Option<u64>,
    ) -> Result<(), engram_core::SandboxError> {
        let dest = self.path.join(name);
        crate::disk::clone_or_copy_blocking(source, &dest)?;
        if !std::fs::symlink_metadata(&dest)?.is_file() {
            return Err(io::Error::other("managed artifact is not a regular file").into());
        }
        verify_file(&dest, digest, size)?;
        Ok(())
    }
}
impl Drop for PinnedFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    #[tokio::test]
    async fn proxy_stop_is_observed_before_any_helper_or_packet_work() {
        use tokio::io::AsyncWriteExt;
        let (client, mut proxy) = tokio::net::UnixStream::pair().unwrap();
        proxy.shutdown().await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                wait_for_private_stop(&client)
            )
            .await
            .is_ok(),
            "proxy drain must not wait for disk pinning"
        );
    }

    fn inputs(
        dir: &Path,
    ) -> (
        crate::backend::VzConfig,
        engram_core::types::managed_cold::ManagedColdPrepare,
        engram_core::types::sandbox::SandboxSpec,
        ManagedHelperArtifact,
    ) {
        use engram_core::types::{
            managed_cold::{ManagedColdDisk, ManagedColdPrepare, ManagedColdSource},
            manifest::ManifestRef,
            sandbox::AuxRoDrive,
        };
        let hash = |s: &[u8]| hex::encode(Sha256::digest(s));
        for (name, bytes) in [
            ("kernel", b"kernel-a".as_slice()),
            ("disk", b"disk-a"),
            ("helper", b"helper-a"),
        ] {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        let bundle = hash(b"bundle-a");
        std::fs::write(dir.join(format!("{bundle}.squashfs")), b"bundle-a").unwrap();
        // The managed path must not resolve the current host's symbolic stamp.
        std::fs::write(dir.join("current.json"), b"invalid-current-stamp").unwrap();
        let drives: Vec<_> = (0..3)
            .map(|i| AuxRoDrive {
                drive_id: AuxRoDrive::slot_drive_id(i),
                guest_mount: AuxRoDrive::slot_guest_mount(i),
                fs_type: "squashfs".into(),
                sha256: Some(bundle.clone()),
            })
            .collect();
        let manifest = ManifestRef::new();
        let spec: engram_core::types::sandbox::SandboxSpec =
            serde_json::from_value(serde_json::json!({
                "image": "image-v1", "rootfs_source": dir.join("disk"), "rootfs_manifest": manifest,
                "cpu": {"vcpus": 1}, "memory": {"max_mib": 128}, "disk": {"max_gib": 1},
                "env": {}, "aux_ro_drives": drives,
            }))
            .unwrap();
        let request = ManagedColdPrepare {
            version: 1,
            session_id: engram_core::SessionId::new(),
            sandbox_id: engram_core::SandboxId::new(),
            boot_attempt: "boot-1".into(),
            image: "image-v1".into(),
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
        (
            crate::backend::VzConfig::with_kernel(dir.join("kernel")).with_bundle_dir(dir),
            request,
            spec,
            ManagedHelperArtifact {
                path: dir.join("helper"),
                sha256: hash(b"helper-a"),
            },
        )
    }

    #[test]
    fn all_selected_boot_files_are_private_and_failed_attempts_leave_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, request, spec, artifact) = inputs(dir.path());
        request.validate(&spec).unwrap();
        let work = dir.path().join("work");
        let files = PinnedFiles::prepare(&work, &cfg, &request, &spec, &artifact).unwrap();
        let path = files.path.clone();
        assert_eq!(std::fs::read(path.join("kernel")).unwrap(), b"kernel-a");
        assert_eq!(std::fs::read(path.join("netstack")).unwrap(), b"helper-a");
        let name = format!(
            "{}.squashfs",
            request.aux_ro_drives[1].sha256.as_ref().unwrap()
        );
        assert_eq!(std::fs::read(path.join(name)).unwrap(), b"bundle-a");
        assert!(PinnedFiles::prepare(&work, &cfg, &request, &spec, &artifact).is_err());
        assert!(
            path.join("rootfs.ext4").exists(),
            "collision must not remove the first owner"
        );
        drop(files);
        assert!(!path.exists());
        std::fs::write(&cfg.kernel_path, b"kernel-b").unwrap();
        assert!(PinnedFiles::prepare(&work, &cfg, &request, &spec, &artifact).is_err());
        assert!(
            !path.exists(),
            "failed pin must remove the private partial copy"
        );
        assert_eq!(std::fs::read(&cfg.kernel_path).unwrap(), b"kernel-b");
    }

    #[tokio::test]
    async fn allocation_mismatch_stops_before_artifacts_or_helper() {
        use engram_egress_proto::private::{self, Allocation, Reply};
        let dir = tempfile::tempdir().unwrap();
        let (cfg, request, spec, artifact) = inputs(dir.path());
        let allocation = Allocation {
            version: 1,
            host_id: engram_core::HostId::new(),
            host_incarnation: "host-1".into(),
            proxy_incarnation: "proxy-1".into(),
            session_id: request.session_id,
            sandbox_id: engram_core::SandboxId::new(),
            boot_attempt: request.boot_attempt.clone(),
            attachment_generation: 1,
            guest_ip: "192.0.2.2".parse().unwrap(),
            gateway_ip: "192.0.2.1".parse().unwrap(),
            policy_revision: 1,
            policy_digest: format!("sha256:{}", "a".repeat(64)),
        };
        let (client, mut server) = tokio::net::UnixStream::pair().unwrap();
        let peer = tokio::spawn(async move {
            let _: engram_egress_proto::ToProxyd =
                engram_egress_proto::read_frame(&mut server).await.unwrap();
            let _: Allocation = private::read(&mut server).await.unwrap();
            private::write(
                &mut server,
                &Reply {
                    version: 1,
                    sequence: 0,
                    epoch: 0,
                    state: "closed".into(),
                },
            )
            .await
            .unwrap();
            server
        });
        let capability = private::register(client, &allocation).await.unwrap();
        let mut peer = peer.await.unwrap();
        let work = dir.path().join("work");
        let result = prepare(&work, &cfg, request, spec, capability, artifact).await;
        assert!(matches!(
            result,
            Err(engram_core::SandboxError::InvalidSpec(_))
        ));
        assert!(!work.exists());
        use tokio::io::AsyncReadExt;
        assert_eq!(
            peer.read(&mut [0]).await.unwrap(),
            0,
            "rejected capability must close"
        );
    }

    #[test]
    fn pinned_disk_survives_source_replacement_and_rejects_changed_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, b"disk-a").unwrap();
        let owned = dir.path().join("private");
        std::fs::create_dir(&owned).unwrap();
        let files = PinnedFiles {
            path: owned.clone(),
        };
        let digest = hex::encode(Sha256::digest(b"disk-a"));
        files.pin(&source, "rootfs.ext4", &digest, Some(6)).unwrap();
        std::fs::write(&source, b"disk-b").unwrap();
        assert_eq!(std::fs::read(owned.join("rootfs.ext4")).unwrap(), b"disk-a");
        assert!(files.pin(&source, "changed", &digest, Some(6)).is_err());
        drop(files);
        assert!(!owned.exists());
        assert_eq!(std::fs::read(source).unwrap(), b"disk-b");
    }

    #[test]
    fn materialized_bytes_must_match_selected_disk_even_at_same_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk");
        std::fs::write(&path, b"disk-a").unwrap();
        let digest = hex::encode(Sha256::digest(b"disk-a"));
        assert!(verify_file(&path, &digest, Some(6)).is_ok());
        std::fs::write(&path, b"disk-b").unwrap();
        assert!(
            verify_file(&path, &digest, Some(6)).is_err(),
            "same size is not byte identity"
        );
        assert!(verify_file(&path, &digest, Some(7)).is_err());
    }
}

mod helper;
