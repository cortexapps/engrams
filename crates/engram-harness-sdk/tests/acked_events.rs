use std::collections::VecDeque;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use engram_harness_proto::attach_token::AttachToken;
use engram_harness_proto::{
    read_msg, write_msg, HarnessAttach, HarnessAttachAck, HarnessCommand, HarnessEvent,
    HarnessFrame,
};
use engram_harness_sdk::{
    serve_with_token, BoxFuture, BoxedReader, BoxedWriter, Channels, ConnectionConfig, UNACKED_MAX,
};
use engram_ids::{SandboxId, SessionId};
use tokio::io::DuplexStream;
use tokio::sync::{mpsc, Notify};

fn cfg() -> ConnectionConfig {
    ConnectionConfig {
        connect: None,
        port: None,
        session_id: SessionId::new(),
        harness_version: "outbox-test".into(),
    }
}
fn token() -> AttachToken {
    AttachToken {
        sandbox_id: SandboxId::new(),
        binding_epoch: 1,
    }
}
fn dialer(
    streams: Vec<DuplexStream>,
) -> impl FnMut() -> BoxFuture<Option<(BoxedReader, BoxedWriter)>> {
    let mut streams: VecDeque<_> = streams.into();
    move || {
        let stream = streams.pop_front();
        Box::pin(async move {
            stream.map(|stream| {
                let (r, w) = tokio::io::split(stream);
                (Box::new(r) as BoxedReader, Box::new(w) as BoxedWriter)
            })
        })
    }
}
async fn attach(host: &mut DuplexStream) {
    let _: HarnessAttach = read_msg(host).await.unwrap();
    write_msg(
        host,
        &HarnessAttachAck {
            ok: true,
            reject: None,
            message: None,
        },
    )
    .await
    .unwrap();
}
async fn event(host: &mut DuplexStream) -> (u64, HarnessEvent) {
    match read_msg(host).await.unwrap() {
        HarnessFrame::SeqEvent { seq, event, .. } => (seq, event),
        other => panic!("expected sequenced event: {other:?}"),
    }
}
async fn ack(host: &mut DuplexStream, seq: u64) {
    write_msg(host, &HarnessFrame::EventAck { seq })
        .await
        .unwrap();
}
async fn shutdown(host: &mut DuplexStream) {
    write_msg(
        host,
        &HarnessFrame::Command(HarnessCommand::Shutdown { grace_secs: 0 }),
    )
    .await
    .unwrap();
    let result = read_msg::<_, HarnessFrame>(host).await;
    assert!(result.is_err(), "no extra event after shutdown: {result:?}");
}

#[tokio::test(start_paused = true)]
async fn events_are_sequenced_and_replayed_until_acked() {
    let (client1, mut host1) = tokio::io::duplex(4096);
    let (client2, mut host2) = tokio::io::duplex(4096);
    let channels = Channels::new();
    let mut commands = channels.command_rx;
    let engine = tokio::spawn(async move {
        for event in [HarnessEvent::Busy, HarnessEvent::Idle, HarnessEvent::Parked] {
            channels.event_tx.send(event).await.unwrap();
        }
        commands.recv().await;
        ExitCode::SUCCESS
    });
    let host = tokio::spawn(async move {
        attach(&mut host1).await;
        assert_eq!(event(&mut host1).await, (1, HarnessEvent::Busy));
        ack(&mut host1, 1).await;
        assert_eq!(event(&mut host1).await, (2, HarnessEvent::Idle));
        assert_eq!(event(&mut host1).await, (3, HarnessEvent::Parked));
        drop(host1);
        attach(&mut host2).await;
        assert_eq!(event(&mut host2).await, (2, HarnessEvent::Idle));
        assert_eq!(event(&mut host2).await, (3, HarnessEvent::Parked));
        ack(&mut host2, 3).await;
        shutdown(&mut host2).await;
    });
    assert_eq!(
        serve_with_token(
            cfg(),
            dialer(vec![client1, client2]),
            token(),
            engine,
            channels.command_tx,
            channels.event_rx,
            channels.reattach
        )
        .await,
        ExitCode::SUCCESS
    );
    host.await.unwrap();
}

#[tokio::test]
async fn outbox_caps_unacked_and_backpressures_engine() {
    let (client, mut host) = tokio::io::duplex(4096);
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let (event_tx, event_rx) = mpsc::channel(1);
    let sent = Arc::new(AtomicUsize::new(0));
    let sent_engine = sent.clone();
    let ready = Arc::new(Notify::new());
    let ready_engine = ready.clone();
    let engine = tokio::spawn(async move {
        for _ in 0..UNACKED_MAX + 2 {
            event_tx.send(HarnessEvent::Busy).await.unwrap();
            let n = sent_engine.fetch_add(1, Ordering::SeqCst) + 1;
            if n == UNACKED_MAX + 1 {
                ready_engine.notify_one();
            }
        }
        command_rx.recv().await;
        ExitCode::SUCCESS
    });
    let host_task = tokio::spawn(async move {
        attach(&mut host).await;
        for seq in 1..=UNACKED_MAX as u64 {
            assert_eq!(event(&mut host).await, (seq, HarnessEvent::Busy));
        }
        ready.notified().await;
        tokio::task::yield_now().await;
        assert_eq!(sent.load(Ordering::SeqCst), UNACKED_MAX + 1);
        // The outbox is full; one more event fits only in the engine channel.
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            read_msg::<_, HarnessFrame>(&mut host)
        )
        .await
        .is_err());
        ack(&mut host, UNACKED_MAX as u64).await;
        for seq in UNACKED_MAX as u64 + 1..=UNACKED_MAX as u64 + 2 {
            assert_eq!(event(&mut host).await, (seq, HarnessEvent::Busy));
        }
        ack(&mut host, UNACKED_MAX as u64 + 2).await;
        shutdown(&mut host).await;
    });
    assert_eq!(
        serve_with_token(
            cfg(),
            dialer(vec![client]),
            token(),
            engine,
            command_tx,
            event_rx,
            Arc::new(Notify::new())
        )
        .await,
        ExitCode::SUCCESS
    );
    host_task.await.unwrap();
}
