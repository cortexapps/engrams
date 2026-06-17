//! Regression: a `Prompt` sent while the harness connection is bouncing
//! must NOT be lost — it must be re-delivered to the next connection.
//!
//! ADR 0052 wedge (prod session `8c165749`): the host→harness command
//! channel was fire-and-forget. `send_prompt` buffered the frame into the
//! live connection's writer and returned `Ok` (the coord then emitted the
//! user echo), but if that connection dropped before the frame was
//! processed — which happens routinely: checkpoints, live moves,
//! idle-evict, the SIGUSR1 reconnect nudge — the prompt died with the
//! connection and was never re-delivered. The session wedged with a
//! forever-greyed bubble: no `RunStarted` and no `PromptQueued` ever came.
//!
//! Events UP are at-least-once (the `held` slot in the harness's
//! `pump_events`); this test pins the symmetric guarantee for commands
//! DOWN. The hub holds an un-confirmed `Prompt` per sandbox and replays it
//! on the next attach; the harness dedupes by `prompt_id`, so a prompt
//! that WAS processed (its confirming event lost in the bounce) is a no-op
//! on replay.

use std::sync::Arc;
use std::time::Duration;

use engram_core::{SandboxId, SessionId};
use engram_harness_proto::{
    read_msg, write_msg, HarnessAttach, HarnessAttachAck, HarnessCommand, HarnessEvent,
    HarnessFrame,
};
use engram_host_agent::harness::{EventSink, HarnessHub};
use tokio::io::{split, AsyncRead, AsyncWrite};

fn noop_sink() -> EventSink {
    Arc::new(move |_session_id, _sandbox_id, _ev: HarnessEvent| Box::new(Box::pin(async move {})))
}

/// Drive the harness side of the handshake on a fresh duplex end:
/// send `HarnessAttach`, await the host's `HarnessAttachAck`.
async fn attach<S>(
    stream: S,
    session_id: SessionId,
) -> (impl AsyncRead + Unpin, impl AsyncWrite + Unpin)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut r, mut w) = split(stream);
    write_msg(
        &mut w,
        &HarnessAttach {
            session_id,
            harness_version: "fake-harness/test".into(),
        },
    )
    .await
    .expect("attach write");
    let ack: HarnessAttachAck = read_msg(&mut r).await.expect("ack read");
    assert!(ack.ok, "host rejected attach: {:?}", ack.message);
    (r, w)
}

async fn wait_attached_count(hub: &HarnessHub, want: usize) {
    for _ in 0..400 {
        if hub.attached_count() == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for attached_count == {want}");
}

#[tokio::test]
async fn prompt_sent_during_connection_bounce_is_redelivered_on_reattach() {
    let hub = HarnessHub::new(noop_sink());
    let sandbox_id = SandboxId::new();
    let session_id = SessionId::new();

    // --- C1: attach, then a prompt arrives ---
    let (host1, harness1) = tokio::io::duplex(1 << 16);
    hub.accept_connection(sandbox_id, Some(session_id), host1);
    let (_r1, _w1) = attach(harness1, session_id).await;
    wait_attached_count(&hub, 1).await;

    // The coordinator forwards a user prompt. On the real path this also
    // emits the durable `role:user` echo — so a loss here strands the user.
    hub.send_prompt(sandbox_id, "p1".into(), "Hi".into())
        .await
        .expect("send_prompt buffered into the live connection");

    // --- The bounce: C1 drops before the fake harness ever reads the
    // Prompt frame (modelling a checkpoint / live-move / idle-evict /
    // SIGUSR1 reconnect coincident with the send). Dropping the harness
    // end EOFs the host's reader → C1 tears down. ---
    drop(_r1);
    drop(_w1);
    wait_attached_count(&hub, 0).await;

    // --- C2: the harness re-dials. The un-confirmed prompt MUST arrive
    // here (no `RunStarted`/`PromptQueued` was ever emitted, so the hub
    // can't consider it delivered). ---
    let (host2, harness2) = tokio::io::duplex(1 << 16);
    hub.accept_connection(sandbox_id, Some(session_id), host2);
    let (mut r2, _w2) = attach(harness2, session_id).await;

    let frame = tokio::time::timeout(Duration::from_secs(3), read_msg::<_, HarnessFrame>(&mut r2))
        .await
        .expect("a prompt should be re-delivered on reattach — not lost in the bounce")
        .expect("frame read");

    match frame {
        HarnessFrame::Command(HarnessCommand::Prompt { prompt_id, text }) => {
            assert_eq!(prompt_id, "p1", "re-delivered the wrong prompt");
            assert_eq!(text, "Hi");
        }
        other => panic!("expected re-delivered Prompt, got {other:?}"),
    }
}
