//! Actual loopback TLS tests. No worker, host, VM or deployment is started.
use engram_coordinator::mcp::operator::{server, OperatorTls};
use engram_core::traits::MetadataStore;
use engram_protocol::mcp as wire;
use engram_sim::{ManualClock, SimEntropy, SimMetadataStore};
use std::{path::Path, sync::Arc};
struct Certs {
    _dir: tempfile::TempDir,
    ca: Vec<u8>,
    server: Vec<u8>,
    server_key: Vec<u8>,
    operator: Vec<u8>,
    operator_key: Vec<u8>,
    other: Vec<u8>,
    other_key: Vec<u8>,
    pin: String,
    other_pin: String,
}
fn certs() -> Certs {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        assert!(std::process::Command::new("openssl")
            .args(args)
            .current_dir(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success());
    };
    run(&[
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
        "/CN=fixture-root",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
    ]);
    std::fs::write(dir.path().join("ext"),"basicConstraints=critical,CA:FALSE\nsubjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth,clientAuth\n").unwrap();
    for name in ["server", "operator", "other"] {
        run(&[
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            &format!("{name}.key"),
            "-out",
            &format!("{name}.csr"),
            "-subj",
            "/CN=localhost",
        ]);
        run(&[
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
            "ext",
        ]);
    }
    fn read(dir: &Path, name: &str) -> Vec<u8> {
        std::fs::read(dir.join(name)).unwrap()
    }
    let pin = |name: &str| {
        let out = std::process::Command::new("openssl")
            .args(["x509", "-in", name, "-noout", "-fingerprint", "-sha256"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .split_once('=')
            .unwrap()
            .1
            .to_owned()
    };
    Certs {
        ca: read(dir.path(), "ca.pem"),
        server: read(dir.path(), "server.pem"),
        server_key: read(dir.path(), "server.key"),
        operator: read(dir.path(), "operator.pem"),
        operator_key: read(dir.path(), "operator.key"),
        other: read(dir.path(), "other.pem"),
        other_key: read(dir.path(), "other.key"),
        pin: pin("operator.pem"),
        other_pin: pin("other.pem"),
        _dir: dir,
    }
}
#[tokio::test]
async fn operator_listener_authenticates_real_peer_and_configured_candidate() {
    let f = certs();
    let meta = SimMetadataStore::new(ManualClock::new(), Arc::new(SimEntropy::seeded(831)));
    let candidate = engram_testkit::operator::candidate();
    let tls = || OperatorTls {
        certificate: f.server.clone(),
        private_key: f.server_key.clone(),
        client_ca: f.ca.clone(),
        fingerprints: vec![f.pin.clone()],
    };
    assert!(
        server(
            meta.clone(),
            candidate.clone(),
            tls(),
            std::slice::from_ref(&f.pin)
        )
        .is_err(),
        "operator and orchestrator leaf identities must differ"
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, done) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(
        server(
            meta.clone(),
            candidate.clone(),
            tls(),
            std::slice::from_ref(&f.other_pin),
        )
        .unwrap()
        .serve_with_incoming_shutdown(
            tonic::transport::server::TcpIncoming::from(listener),
            async {
                let _ = done.await;
            },
        ),
    );
    let endpoint =
        tonic::transport::Endpoint::from_shared(format!("https://localhost:{}", addr.port()))
            .unwrap()
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(2));
    let base = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(&f.ca))
        .domain_name("localhost");
    if let Ok(channel) = endpoint
        .clone()
        .tls_config(base.clone())
        .unwrap()
        .connect()
        .await
    {
        assert!(
            wire::operator_publication_service_client::OperatorPublicationServiceClient::new(
                channel
            )
            .read(wire::OperatorPublicationServiceReadRequest { protocol: 1 })
            .await
            .is_err()
        );
    }
    let channel = endpoint
        .clone()
        .tls_config(
            base.clone()
                .identity(tonic::transport::Identity::from_pem(&f.other, &f.other_key)),
        )
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut denied =
        wire::operator_publication_service_client::OperatorPublicationServiceClient::new(channel);
    let mut forged = tonic::Request::new(wire::OperatorPublicationServiceStageRequest {
        protocol: 1,
        expected_generation: None,
        configured_candidate_identity: candidate.identity().unwrap(),
    });
    forged
        .metadata_mut()
        .insert("x-operator-identity", f.pin.parse().unwrap());
    assert_eq!(
        denied.stage(forged).await.unwrap_err().code(),
        tonic::Code::PermissionDenied
    );
    assert!(meta
        .read_operator_publication()
        .await
        .unwrap()
        .pending
        .is_none());
    let channel = endpoint
        .tls_config(base.identity(tonic::transport::Identity::from_pem(
            &f.operator,
            &f.operator_key,
        )))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut client =
        wire::operator_publication_service_client::OperatorPublicationServiceClient::new(channel);
    assert_eq!(
        client
            .read(wire::OperatorPublicationServiceReadRequest { protocol: 0 })
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
    assert_eq!(
        client
            .stage(wire::OperatorPublicationServiceStageRequest {
                protocol: 1,
                expected_generation: None,
                configured_candidate_identity: engram_core::types::managed_create::digest(
                    b"unconfigured input"
                )
            })
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    let input = wire::OperatorPublicationServiceStageRequest {
        protocol: 1,
        expected_generation: None,
        configured_candidate_identity: candidate.identity().unwrap(),
    };
    let first = client.stage(input.clone()).await.unwrap().into_inner();
    let second = client.stage(input).await.unwrap().into_inner();
    assert_eq!(first.publication_json, second.publication_json);
    let state: engram_core::types::managed_operator::OperatorPublication =
        serde_json::from_slice(&first.publication_json).unwrap();
    assert!(!state.admission_open);
    assert!(state.published.is_none());
    assert_eq!(state.pending.unwrap().candidate, candidate);
    assert_eq!(
        client
            .read(wire::OperatorPublicationServiceReadRequest { protocol: 1 })
            .await
            .unwrap()
            .into_inner()
            .publication_json,
        first.publication_json
    );
    drop(client);
    drop(denied);
    let _ = stop.send(());
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
