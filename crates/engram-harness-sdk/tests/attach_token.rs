use std::process::ExitCode;
use std::time::Duration;

use engram_harness_proto::attach_token::*;
use engram_harness_proto::{read_msg, write_msg, AttachReject, HarnessAttach, HarnessAttachAck};
use engram_harness_sdk::{
    serve_with, BoxedReader, BoxedWriter, Channels, ConnectionConfig, SUPERSEDED_TOKEN_GRACE,
};
use engram_ids::{SandboxId, SessionId};

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
struct TokenEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl TokenEnv {
    fn set(path: &std::path::Path, token: AttachToken) -> Self {
        let old = [ATTACH_TOKEN_FILE_ENV, SANDBOX_ID_ENV, BINDING_EPOCH_ENV]
            .map(|key| (key, std::env::var_os(key)))
            .to_vec();
        std::env::set_var(ATTACH_TOKEN_FILE_ENV, path);
        for (k, v) in token.env() {
            std::env::set_var(k, v);
        }
        Self(old)
    }
}
impl Drop for TokenEnv {
    fn drop(&mut self) {
        for (k, v) in &self.0 {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}
fn token(epoch: u64) -> AttachToken {
    AttachToken {
        sandbox_id: SandboxId::new(),
        binding_epoch: epoch,
    }
}
fn cfg() -> ConnectionConfig {
    ConnectionConfig {
        connect: None,
        port: None,
        session_id: SessionId::new(),
        harness_version: "test".into(),
    }
}

async fn assert_attach_token(expected: AttachToken) {
    let (client, mut host) = tokio::io::duplex(4096);
    let channels = Channels::new();
    let mut commands = channels.command_rx;
    let engine = tokio::spawn(async move {
        let _events = channels.event_tx;
        commands.recv().await;
        ExitCode::SUCCESS
    });
    let host_task = tokio::spawn(async move {
        let attach: HarnessAttach = read_msg(&mut host).await.unwrap();
        assert_eq!(attach.sandbox_id, expected.sandbox_id);
        assert_eq!(attach.binding_epoch, expected.binding_epoch);
        write_msg(
            &mut host,
            &HarnessAttachAck {
                ok: true,
                reject: None,
                message: None,
            },
        )
        .await
        .unwrap();
        write_msg(
            &mut host,
            &engram_harness_proto::HarnessFrame::Command(
                engram_harness_proto::HarnessCommand::Shutdown { grace_secs: 0 },
            ),
        )
        .await
        .unwrap();
        // Keep the connection alive until the SDK closes it.
        let _ = read_msg::<_, engram_harness_proto::HarnessFrame>(&mut host).await;
    });
    let mut stream = Some(client);
    assert_eq!(
        serve_with(
            cfg(),
            move || {
                let stream = stream.take();
                Box::pin(async move {
                    stream.map(|stream| {
                        let (r, w) = tokio::io::split(stream);
                        (Box::new(r) as BoxedReader, Box::new(w) as BoxedWriter)
                    })
                })
            },
            engine,
            channels.command_tx,
            channels.event_rx,
            channels.reattach
        )
        .await,
        ExitCode::SUCCESS
    );
    host_task.await.unwrap();
}

#[tokio::test]
async fn token_file_wins_over_env() {
    let _lock = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let _env = TokenEnv::set(&path, token(1));
    let file_token = token(2);
    file_token.write_atomic(&path).unwrap();
    assert_attach_token(file_token).await;
}

#[tokio::test]
async fn token_falls_back_to_env_when_file_missing() {
    let _lock = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let expected = token(1);
    let _env = TokenEnv::set(&dir.path().join("missing"), expected);
    assert_attach_token(expected).await;
}

async fn superseded_case(update: bool) {
    let _lock = ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    let original = token(1);
    let _env = TokenEnv::set(&path, original);
    original.write_atomic(&path).unwrap();
    let newer = AttachToken {
        binding_epoch: 2,
        ..original
    };
    let channels = Channels::new();
    let mut commands = channels.command_rx;
    let events = channels.event_tx;
    let engine = tokio::spawn(async move {
        while commands.recv().await.is_some() {}
        drop(events);
        ExitCode::SUCCESS
    });
    let (client1, mut host1) = tokio::io::duplex(4096);
    let (client2, mut host2) = tokio::io::duplex(4096);
    let host = tokio::spawn(async move {
        let attach: HarnessAttach = read_msg(&mut host1).await.unwrap();
        assert_eq!(attach.binding_epoch, 1);
        write_msg(
            &mut host1,
            &HarnessAttachAck {
                ok: false,
                reject: Some(AttachReject::Superseded),
                message: None,
            },
        )
        .await
        .unwrap();
        if update {
            // Exercise the poll path inside the grace period.
            tokio::time::sleep(Duration::from_secs(1)).await;
            newer.write_atomic(&path).unwrap();
            let attach: HarnessAttach = read_msg(&mut host2).await.unwrap();
            assert_eq!(attach.binding_epoch, 2);
            assert_eq!(attach.sandbox_id, newer.sandbox_id);
            // A second true fence lets the engine close cleanly.
            write_msg(
                &mut host2,
                &HarnessAttachAck {
                    ok: false,
                    reject: Some(AttachReject::Superseded),
                    message: None,
                },
            )
            .await
            .unwrap();
        }
    });
    let mut streams = std::collections::VecDeque::from([client1, client2]);
    let mut dials = 0;
    let start = tokio::time::Instant::now();
    let result = serve_with(
        cfg(),
        || {
            dials += 1;
            let stream = streams.pop_front();
            Box::pin(async move {
                stream.map(|s| {
                    let (r, w) = tokio::io::split(s);
                    (Box::new(r) as BoxedReader, Box::new(w) as BoxedWriter)
                })
            })
        },
        engine,
        channels.command_tx,
        channels.event_rx,
        channels.reattach,
    )
    .await;
    assert_eq!(result, ExitCode::SUCCESS);
    host.await.unwrap();
    assert_eq!(dials, if update { 2 } else { 1 });
    if !update {
        assert_eq!(start.elapsed(), SUPERSEDED_TOKEN_GRACE);
    }
}

#[tokio::test(start_paused = true)]
async fn superseded_with_newer_token_file_redials_with_new_epoch() {
    superseded_case(true).await;
}
#[tokio::test(start_paused = true)]
async fn superseded_without_newer_token_exits_after_grace() {
    superseded_case(false).await;
}
