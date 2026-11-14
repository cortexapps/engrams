#![cfg(unix)]

use engram_egress_proto::{
    adopt::{decide_adopt, AdoptPlan, ExpectedProxyd, LiveEvidence},
    manifest::{ProcIdentity, ProxydManifest},
    read_frame, write_frame, FromProxyd, HelloInfo,
};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};
use tokio::{io::AsyncWriteExt, net::UnixStream, time::timeout};

// Protocol 1 HelloAck from ec526661; the frame body is 180 bytes.
const PREDECESSOR_HEADER: &[u8] = b"\0\0\0\xb4";
const PREDECESSOR_BODY: &[u8] = br#"{"HelloAck":{"proto_version":1,"source_fingerprint":"same-build","proxy_port":8443,"dns_port":5353,"gateway_port":13338,"ca_fingerprint":"test-ca","coord_url":"http://coord:8080"}}"#;

#[tokio::test]
async fn predecessor_hello_selects_replacement_before_policy_replay() {
    timeout(Duration::from_secs(5), async {
        let (mut daemon, mut owner) = UnixStream::pair().unwrap();
        daemon.write_all(PREDECESSOR_HEADER).await.unwrap();
        daemon.write_all(PREDECESSOR_BODY).await.unwrap();
        let FromProxyd::HelloAck(hello) = read_frame(&mut owner).await.unwrap() else {
            panic!("expected predecessor HelloAck");
        };
        assert!(hello.incarnation.is_empty());
        let manifest = ProxydManifest {
            schema_version: 1,
            pid: 100,
            start_time_jiffies: 5,
            comm: "engram-egress-p".into(),
            source_fingerprint: Some("same-build".into()),
            proxy_port: 8443,
            dns_port: 5353,
            gateway_port: 13338,
            control_sock: PathBuf::from("/test/egress-proxyd.sock"),
        };
        let evidence = LiveEvidence {
            identity: Some(ProcIdentity {
                pid: 100,
                start_time_jiffies: 5,
                comm: "engram-egress-p".into(),
            }),
            hello: Some(hello),
            probe_ok: true,
        };
        let expected = ExpectedProxyd {
            source_fingerprint: Some("same-build"),
            proxy_port: 8443,
            dns_port: 5353,
            gateway_port: 13338,
            ca_fingerprint: "test-ca",
            coord_url: "http://coord:8080",
        };
        assert_eq!(
            decide_adopt(Some(&manifest), &evidence, &expected),
            AdoptPlan::KillStaleAndSpawn
        );
    })
    .await
    .expect("predecessor Hello exchange timed out");
}

// The HelloAck variant and HelloInfo fields from ec526661.
#[derive(Deserialize)]
enum PredecessorReply {
    HelloAck(PredecessorHelloInfo),
}

#[derive(Deserialize, Serialize)]
struct PredecessorHelloInfo {
    proto_version: u32,
    source_fingerprint: Option<String>,
    proxy_port: u16,
    dns_port: u16,
    gateway_port: u16,
    ca_fingerprint: String,
    coord_url: String,
}

#[tokio::test]
async fn predecessor_owner_can_read_current_hello_protocol_mismatch() {
    timeout(Duration::from_secs(5), async {
        let (mut daemon, mut owner) = UnixStream::pair().unwrap();
        write_frame(
            &mut daemon,
            &FromProxyd::HelloAck(HelloInfo {
                incarnation: "current-daemon".into(),
                proto_version: 5,
                source_fingerprint: Some("current-build".into()),
                proxy_port: 8443,
                dns_port: 5353,
                gateway_port: 13338,
                ca_fingerprint: "test-ca".into(),
                coord_url: "http://coord:8080".into(),
            }),
        )
        .await
        .unwrap();
        let PredecessorReply::HelloAck(hello) = read_frame(&mut owner).await.unwrap();
        assert_eq!(
            serde_json::to_value(hello).unwrap(),
            serde_json::json!({
                "proto_version": 5,
                "source_fingerprint": "current-build",
                "proxy_port": 8443,
                "dns_port": 5353,
                "gateway_port": 13338,
                "ca_fingerprint": "test-ca",
                "coord_url": "http://coord:8080",
            })
        );
    })
    .await
    .expect("current Hello exchange timed out");
}
