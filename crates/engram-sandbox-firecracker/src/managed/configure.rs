use super::network::{NetworkShape, GUEST_MAC, TAP};
use crate::{FirecrackerClient, FirecrackerConfig};
use engram_core::{
    types::{managed_cold::ManagedColdPrepare, sandbox::SandboxSpec},
    SandboxError,
};
use std::path::Path;

pub(super) async fn configure(
    api: &FirecrackerClient,
    cfg: &FirecrackerConfig,
    request: &ManagedColdPrepare,
    spec: &SandboxSpec,
    path: &Path,
    shape: &NetworkShape,
    cid: u32,
) -> Result<(), SandboxError> {
    use crate::{BootSource, DriveConfig, MachineConfig, NetworkInterface, VsockConfig};
    api.put_machine_config(&MachineConfig {
        vcpu_count: spec
            .cpu
            .vcpus
            .try_into()
            .map_err(|_| SandboxError::InvalidSpec("invalid CPU count".into()))?,
        mem_size_mib: spec.memory.max_mib,
        smt: false,
        track_dirty_pages: cfg.track_dirty_pages,
        cpu_template: cfg.cpu_template.clone(),
    })
    .await?;
    // Reject a second kernel networking declaration. Guest configuration gives
    // function; the namespace and nft rules provide enforcement.
    if cfg.default_boot_args.split_whitespace().any(|arg| {
        arg.starts_with("ip=")
            || arg.starts_with("engram_managed_dns=")
            || arg == "engram_base_capture=1"
    }) {
        return Err(SandboxError::InvalidSpec(
            "managed boot arguments conflict with selected network".into(),
        ));
    }
    let boot_args = format!(
        "{} ip={}::{}:255.255.255.0::eth0:off:{} engram_managed_dns={}",
        cfg.default_boot_args.trim_end(),
        shape.guest,
        shape.gateway,
        shape.gateway,
        shape.gateway
    );
    api.put_boot_source(&BootSource {
        kernel_image_path: path.join("kernel").to_string_lossy().into_owned(),
        boot_args,
        initrd_path: None,
    })
    .await?;
    api.put_drive(&DriveConfig {
        drive_id: "rootfs".into(),
        path_on_host: path.join("rootfs.ext4").to_string_lossy().into_owned(),
        is_root_device: true,
        is_read_only: false,
    })
    .await?;
    for drive in &request.aux_ro_drives {
        let hash = drive
            .sha256
            .as_ref()
            .ok_or_else(|| SandboxError::InvalidSpec("unresolved boot drive".into()))?;
        api.put_drive(&DriveConfig {
            drive_id: drive.drive_id.clone(),
            path_on_host: path
                .join(format!("{hash}.squashfs"))
                .to_string_lossy()
                .into_owned(),
            is_root_device: false,
            is_read_only: true,
        })
        .await?;
    }
    if spec.swap_mib.is_some_and(|n| n > 0) {
        api.put_drive(&DriveConfig {
            drive_id: "swap".into(),
            path_on_host: path.join("swap.img").to_string_lossy().into_owned(),
            is_root_device: false,
            is_read_only: false,
        })
        .await?;
    }
    if cfg.balloon {
        api.put_balloon(&crate::client::BalloonConfig {
            amount_mib: 0,
            deflate_on_oom: true,
            stats_polling_interval_s: 1,
        })
        .await?;
    }
    api.put_network_interface(&NetworkInterface {
        iface_id: "eth0".into(),
        host_dev_name: TAP.into(),
        guest_mac: Some(GUEST_MAC.into()),
    })
    .await?;
    api.put_vsock(&VsockConfig {
        guest_cid: cid,
        uds_path: path.join("guest.vsock").to_string_lossy().into_owned(),
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn cold_configuration_never_starts_or_loads_and_uses_exact_private_files() {
        let dir = tempfile::tempdir().unwrap();
        let (cfg, request, spec) = super::super::files::tests::inputs(dir.path());
        let socket = dir.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut connection, _)) = listener.accept().await else {
                    break;
                };
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    head.push(connection.read_u8().await.unwrap());
                }
                let head = String::from_utf8(head).unwrap();
                let length: usize = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|s| s.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                connection.read_exact(&mut body).await.unwrap();
                tx.send((
                    head.lines().next().unwrap().to_string(),
                    serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                ))
                .await
                .unwrap();
                connection
                    .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .unwrap();
            }
        });
        let shape =
            NetworkShape::new("192.0.2.2".parse().unwrap(), "192.0.2.1".parse().unwrap()).unwrap();
        configure(
            &FirecrackerClient::new(socket),
            &cfg,
            &request,
            &spec,
            dir.path(),
            &shape,
            42,
        )
        .await
        .unwrap();
        let mut sent = Vec::new();
        while let Ok(item) = rx.try_recv() {
            sent.push(item);
        }
        server.abort();
        assert_eq!(
            sent.len(),
            8,
            "cold prepare must configure all devices without start/load"
        );
        assert!(sent.iter().all(|(line, _)| !line.contains("/actions")
            && !line.contains("/snapshot")
            && !line.contains("PATCH")));
        assert_eq!(
            sent[1].1["kernel_image_path"],
            dir.path().join("kernel").to_str().unwrap()
        );
        assert_eq!(
            sent[2].1["path_on_host"],
            dir.path().join("rootfs.ext4").to_str().unwrap()
        );
        assert_eq!(sent[6].1["guest_mac"], "02:00:00:00:00:02");
        assert!(sent[1].1["boot_args"]
            .as_str()
            .unwrap()
            .contains("ip=192.0.2.2::192.0.2.1:255.255.255.0::eth0:off:192.0.2.1"));
    }
}
