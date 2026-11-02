//! Physical file and authenticated transport proof. No VM or proxy is started.
use engram_core::{
    traits::HostClient,
    types::{managed_assets::ManagedColdAssetsRequest, manifest::ManifestRef, sandbox::AuxRoDrive},
    HostId, SessionId,
};
use engram_host_agent::{
    bindings::BindingStore,
    harness::HarnessHub,
    managed_boot::{ManagedBackend, ManagedBootHost, ManagedBootHostConfig},
};
use engram_protocol::{
    grpc_client::GrpcHostClient,
    host_tls::{HostClientTls, HostPeer},
};
use futures::FutureExt;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::Duration,
};

fn openssl(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("openssl")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "certificate fixture generation failed"
    );
}
fn certificates(dir: &Path) {
    openssl(
        dir,
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            "ca.key",
            "-out",
            "ca.pem",
            "-days",
            "1",
            "-subj",
            "/CN=test-ca",
        ],
    );
    std::fs::write(
        dir.join("extensions"),
        "subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth,clientAuth\n",
    )
    .unwrap();
    for name in ["host", "coord", "other"] {
        openssl(
            dir,
            &[
                "req",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                &format!("{name}.key"),
                "-out",
                &format!("{name}.csr"),
                "-subj",
                "/CN=localhost",
            ],
        );
        openssl(
            dir,
            &[
                "x509",
                "-req",
                "-in",
                &format!("{name}.csr"),
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key",
                "-CAcreateserial",
                "-out",
                &format!("{name}.pem"),
                "-days",
                "1",
                "-extfile",
                "extensions",
            ],
        );
        openssl(
            dir,
            &[
                "x509",
                "-in",
                &format!("{name}.pem"),
                "-outform",
                "DER",
                "-out",
                &format!("{name}.der"),
            ],
        );
    }
}

#[tokio::test]
async fn cold_assets_actual_files_require_pinned_mutual_tls() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    certificates(dir);
    std::fs::write(dir.join("kernel"), b"selected kernel").unwrap();
    let host_id = HostId::new();
    let session_id = SessionId::new();
    let ordinary = Arc::new(engram_sandbox_process::ProcessBackend::new(
        dir.join("ordinary"),
    ));
    #[cfg(target_os = "macos")]
    let backend = ManagedBackend::Vz {
        backend: Arc::new(
            engram_sandbox_vz::VzBackend::new(
                dir.join("native"),
                engram_sandbox_vz::VzConfig::with_kernel(dir.join("kernel")).with_bundle_dir(dir),
            )
            .unwrap(),
        ),
        helper: engram_sandbox_vz::ManagedHelperArtifact {
            path: dir.join("absent-helper"),
            sha256: "a".repeat(64),
        },
    };
    #[cfg(target_os = "linux")]
    let backend = {
        let mut config =
            engram_sandbox_firecracker::FirecrackerConfig::with_kernel(dir.join("kernel"));
        config.bundle_dir = dir.to_owned();
        ManagedBackend::Firecracker(Arc::new(
            engram_sandbox_firecracker::FirecrackerBackend::new(dir.join("native"), config),
        ))
    };
    let chunks = engram_chunk_store::ChunkStore::new(Arc::new(
        engram_storage_local::LocalBlobStorage::new(dir.join("blob")),
    ));
    let manifest = ManifestRef::new();
    chunks
        .put_manifest(
            manifest,
            &engram_chunk_store::Manifest::empty(engram_chunk_store::ManifestKind::Disk, 9),
        )
        .await
        .unwrap();
    let drives = (0..3)
        .map(|i| {
            let bytes = [i as u8 + 1; 4];
            let hash = hex::encode(Sha256::digest(bytes));
            std::fs::write(dir.join(AuxRoDrive::staged_file_name(&hash)), bytes).unwrap();
            AuxRoDrive {
                drive_id: AuxRoDrive::slot_drive_id(i),
                guest_mount: AuxRoDrive::slot_guest_mount(i),
                fs_type: "squashfs".into(),
                sha256: Some(hash),
            }
        })
        .collect();
    let request = ManagedColdAssetsRequest {
        version: 1,
        host_id,
        session_id,
        intent_id: "intent-a".into(),
        image: "test.invalid/image:v1".into(),
        image_digest: format!("sha256:{}", "a".repeat(64)),
        disk_manifest: manifest,
        aux_ro_drives: drives,
    };
    let hub = Arc::new(HarnessHub::new(
        engram_host_agent::harness::event_sink_to(|_, _, _, _| async { Ok(()) }),
        BindingStore::open(dir.join("bindings")).unwrap(),
    ));
    let boot = Arc::new(
        ManagedBootHost::new(
            ManagedBootHostConfig {
                host_id,
                incarnation: "tls-host".into(),
                directory: dir.join("records"),
            },
            backend,
            ordinary.clone(),
            Arc::new(engram_host_agent::egress::HostEgress::new(
                Arc::new(engram_host_agent::proxyd_client::ProxydHandle::new(
                    dir.join("absent-proxy.sock"),
                )),
                String::new(),
                None,
            )),
            hub.clone(),
            chunks,
        )
        .unwrap(),
    );
    let local = Arc::new(
        engram_host_agent::host_client::LocalHostClient::new(ordinary, hub).with_managed_boot(boot),
    );
    let expected = local
        .read_managed_cold_assets(request.clone())
        .await
        .unwrap();
    let pins = |name: &str| {
        BTreeSet::from([engram_protocol::host_tls::certificate_pin(
            &std::fs::read(dir.join(format!("{name}.der"))).unwrap(),
        )])
    };
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    let mut server = tokio::spawn(engram_host_agent::grpc_server::boot_with_managed_tls(
        addr,
        local,
        None,
        engram_host_agent::session_epochs::SessionEpochStore::open(dir.join("epochs")).unwrap(),
        None,
        Some(engram_host_agent::managed_tls::ManagedHostTls {
            certificate: dir.join("host.pem"),
            key: dir.join("host.key"),
            client_ca: dir.join("ca.pem"),
            coordinator_pins: pins("coord"),
        }),
    ));
    let address = format!("https://{addr}");
    let mut tls = HostClientTls {
        client_certificate: dir.join("coord.pem"),
        client_key: dir.join("coord.key"),
        hosts: BTreeMap::from([(
            host_id,
            HostPeer {
                endpoint: address.clone(),
                server_name: "localhost".into(),
                server_ca: dir.join("ca.pem"),
                server_pins: pins("host"),
            },
        )]),
    };
    let connect = |config: &HostClientTls| {
        GrpcHostClient::new(
            config
                .endpoint(
                    host_id,
                    &address,
                    tonic::transport::Endpoint::from_shared(address.clone()).unwrap(),
                )
                .unwrap()
                .connect_lazy(),
        )
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        std::panic::AssertUnwindSafe(async {
            loop {
                if connect(&tls).ping().await.is_ok() {
                    break;
                }
                assert!(!server.is_finished());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let receipt = connect(&tls)
                .read_managed_cold_assets(request.clone())
                .await
                .unwrap();
            assert_eq!(receipt, expected);
            assert_eq!(receipt.disk_sha256, hex::encode(Sha256::digest([0; 9])));
            assert_eq!(
                receipt.kernel_sha256,
                hex::encode(Sha256::digest(b"selected kernel"))
            );
            assert_eq!(receipt.host_incarnation, "tls-host");
            let mut changed = request.clone();
            changed.host_id = HostId::new();
            assert!(connect(&tls)
                .read_managed_cold_assets(changed)
                .await
                .is_err());
            tls.client_certificate = dir.join("other.pem");
            tls.client_key = dir.join("other.key");
            assert!(connect(&tls)
                .read_managed_cold_assets(request.clone())
                .await
                .is_err());
            let no_certificate = tonic::transport::Endpoint::from_shared(address.clone())
                .unwrap()
                .tls_config(
                    tonic::transport::ClientTlsConfig::new()
                        .domain_name("localhost")
                        .ca_certificate(tonic::transport::Certificate::from_pem(
                            std::fs::read(dir.join("ca.pem")).unwrap(),
                        )),
                )
                .unwrap()
                .connect_lazy();
            assert!(GrpcHostClient::new(no_certificate)
                .read_managed_cold_assets(request)
                .await
                .is_err());
            assert_eq!(std::fs::read_dir(dir.join("records")).unwrap().count(), 0);
            assert!(!dir.join("native").exists());
            assert!(!dir.join("absent-proxy.sock").exists());
        })
        .catch_unwind(),
    )
    .await;
    // The service's public runner has no graceful stop handle. Drop its exact
    // task, join it, then let the owned certificate and asset directory drop.
    server.abort();
    let stopped = tokio::time::timeout(Duration::from_secs(2), &mut server).await;
    assert!(stopped.is_ok());
    outcome.unwrap().unwrap();
}
