//! ADR 0123 X1: a non-drained snapshot retains the SDK harness and its open run.
#![cfg(target_os = "linux")]

mod common;

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{
    AgentSpec, CpuLimit, DiskLimit, ExecEvent, ExecRequest, MemoryLimit, SandboxSpec,
};
use engram_core::{SandboxId, SessionId};
use engram_harness_proto::{HarnessEvent, HARNESS_VSOCK_PORT};
use engram_host_agent::bindings::BindingStore;
use engram_host_agent::harness::{EventDelivery, EventSink, HarnessHub};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;
use parking_lot::Mutex;
use tokio::time::timeout;

const BUDGET: Duration = Duration::from_secs(30);
const NOOP: &str = "/opt/noop/harness";
const PID: &str = "/run/engram/harness.pid";
type Record = (SessionId, SandboxId, HarnessEvent, Option<EventDelivery>);

async fn until(mut ready: impl FnMut() -> bool) {
    timeout(BUDGET, async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition did not become true within 30 seconds");
}

async fn exec(backend: &PooledBackend, id: SandboxId, command: &str) -> String {
    timeout(BUDGET, async {
        let request = ExecRequest {
            command: vec!["/bin/sh".into(), "-c".into(), command.into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout: Some(BUDGET),
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
        };
        let mut stream = loop {
            match backend.exec_stream(id, request.clone()).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        };
        let mut output = Vec::new();
        while let Some(event) = stream.events.next().await {
            match event {
                ExecEvent::Stdout(bytes) => output.extend(bytes),
                ExecEvent::Exit(code) => {
                    assert_eq!(code, Some(0), "guest command failed: {command}");
                    return String::from_utf8(output).expect("guest output is UTF-8");
                }
                ExecEvent::Refused(reason) => panic!("exec refused: {reason}"),
                ExecEvent::Stderr(bytes) => eprintln!("{}", String::from_utf8_lossy(&bytes)),
            }
        }
        panic!("exec stream closed without exit: {command}");
    })
    .await
    .expect("guest exec timed out")
}

#[tokio::test]
#[ignore = "requires Linux, KVM, Firecracker, and prebuilt musl agentd and noop binaries"]
async fn non_drained_resume_reattaches_the_live_harness_and_finishes_its_run() {
    // An explicitly selected acceptance test must fail if its CI fixtures are absent.
    assert!(Path::new("/dev/kvm").exists(), "KVM is required");
    let kernel = std::env::var("FC_TEST_KERNEL").expect("FC_TEST_KERNEL");
    let busybox = common::find_busybox().expect("static busybox");
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let agentd = target.join("x86_64-unknown-linux-musl/release/engram-agentd");
    let noop = target.join("x86_64-unknown-linux-musl/release/engram-harness-noop");
    assert!(agentd.is_file(), "build the musl agentd binary");
    assert!(noop.is_file(), "build the musl noop binary");

    let work = tempfile::tempdir().expect("host work directory");
    let blob_root = work.path().join("blob");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(blob_root.clone()),
    );
    let chunks = engram_chunk_store::ChunkStore::new(blob);
    let image = common::bake_fixture_ext4(
        &work.path().join("rootfs.ext4"),
        &chunks,
        &busybox,
        Some(InitInjection {
            vsock_port: ENGRAM_AGENTD_PORT,
            transport: Transport::Vsock,
            init_script: None,
        }),
        |tree| {
            std::fs::create_dir_all(tree.join("opt/noop"))?;
            let destination = tree.join("opt/noop/harness");
            std::fs::copy(&noop, &destination)?;
            std::fs::set_permissions(destination, std::fs::Permissions::from_mode(0o755))
        },
    )
    .await;
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agentd);
    let mut config = FirecrackerConfig::with_kernel(kernel);
    config.net_pool = None;
    config.bundle_dir = staged.bundle_dir.clone();
    config.restore_mode = RestoreMode::File;
    config.uffd_handler_bin = target.join("debug/engram-uffd-handler");
    config.uffd_blob_root = Some(blob_root);
    config.uffd_cache_root = Some(work.path().join("chunk-cache"));
    config.track_dirty_pages = true;
    config.default_boot_args =
        "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let mut cache =
        engram_chunk_store::cache::ChunkCacheConfig::new(work.path().join("chunk-cache"));
    cache.budget_bytes = 1024 * 1024 * 1024;
    let pooled = PooledBackend::new(Arc::new(FirecrackerBackend::new(work.path(), config)))
        .with_chunk_store(chunks, work.path().join("materialize"))
        .with_chunk_cache(engram_chunk_store::ChunkCache::new(cache))
        .with_checkpoint_dir(work.path().join("checkpoints"));

    let records = Arc::new(Mutex::new(Vec::<Record>::new()));
    let captured = records.clone();
    let sink: EventSink = Arc::new(move |session, sandbox, event, delivery| {
        let captured = captured.clone();
        Box::pin(async move {
            captured.lock().push((session, sandbox, event, delivery));
            Ok(()) // The real hub sends EventAck only after this return.
        })
    });
    let hub = HarnessHub::new(
        sink,
        BindingStore::open(work.path().join("bindings")).expect("binding store"),
    );
    let accepting = hub.clone();
    pooled.set_harness_sink(Arc::new(move |stream| {
        accepting.accept_via_session_lookup(stream);
    }));
    let source = pooled
        .create(SandboxSpec {
            image: "rehome-retained-harness".into(),
            rootfs_source: Some(image.rootfs_path),
            image_uri: None,
            rootfs_manifest: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: vec![staged.agentd_slot()],
            swap_mib: None,
        })
        .await
        .expect("create source VM");
    exec(&pooled, source, "true").await;
    let session: SessionId = "00000000-0000-0000-0000-000000000123".parse().unwrap();
    let mut agent = AgentSpec {
        binding_epoch: 1,
        argv: vec![
            NOOP.into(),
            "--port".into(),
            HARNESS_VSOCK_PORT.to_string(),
            "--session-id".into(),
            session.to_string(),
            "--no-autorun".into(),
            "--tool-calls".into(),
            "1".into(),
            "--tool-sleep-secs".into(),
            "10".into(),
            "--send-run-completed".into(),
            "--pid-file".into(),
            PID.into(),
        ],
        env: HashMap::new(),
        session_env: HashMap::new(),
        host_ca_pem: None,
    };
    hub.bind_session(session, source, 1).expect("bind epoch 1");
    pooled
        .start_agent(source, agent.clone())
        .await
        .expect("start noop");
    until(|| hub.is_attached(source)).await;
    hub.send_prompt(source, "p1".into(), "run one tool".into(), None)
        .await
        .expect("send p1");
    until(|| {
        let events = records.lock();
        events.iter().any(
            |r| matches!(&r.2, HarnessEvent::RunStarted { prompt_id: Some(p), .. } if p == "p1"),
        ) && events
            .iter()
            .any(|r| matches!(r.2, HarnessEvent::ToolCallStarted { .. }))
    })
    .await;
    let pid_before = exec(&pooled, source, &format!("cat {PID}")).await;
    assert!(pid_before.trim().parse::<u32>().expect("numeric guest PID") > 1);

    // Do not call hub.drain or send Shutdown. Capture the open run through
    // the same pooled snapshot primitive used by the snapshot/evict path.
    let snapshot = pooled.snapshot(source).await.expect("non-drained snapshot");
    pooled
        .destroy(source)
        .await
        .expect("destroy source before restore");
    until(|| !hub.is_attached(source)).await;
    assert!(!pooled.list().await.unwrap().contains(&source));
    let before_restore = records.lock().len();
    assert!(
        records.lock().iter().all(|r| !matches!(
            r.2,
            HarnessEvent::ToolCallCompleted { .. } | HarnessEvent::RunCompleted { .. }
        )),
        "capture must occur while the tool and run are open"
    );

    let restored = pooled
        .restore(snapshot)
        .await
        .expect("restore memory snapshot");
    assert_ne!(restored, source);
    hub.bind_session(session, restored, 2)
        .expect("bind epoch 2");
    agent.binding_epoch = 2;
    // agentd writes the new attach token before it signals the retained child.
    pooled
        .start_agent(restored, agent)
        .await
        .expect("reattach retained noop");
    until(|| hub.is_attached(restored)).await;
    let pid_after = exec(
        &pooled,
        restored,
        &format!("cat {PID}; kill -0 $(cat {PID})"),
    )
    .await;
    assert_eq!(
        pid_after.trim(),
        pid_before.trim(),
        "the harness must not respawn"
    );
    until(|| {
        let events = records.lock();
        let tail = &events[before_restore..];
        tail.iter()
            .any(|r| matches!(r.2, HarnessEvent::RunCompleted { ok: true, .. }))
            && tail.iter().any(|r| matches!(r.2, HarnessEvent::Idle))
    })
    .await;
    // End the observation at a confirmed disconnect, without a quiet-time sleep.
    hub.shutdown(restored, 0)
        .await
        .expect("stop completed harness");
    until(|| !hub.is_attached(restored)).await;
    pooled.destroy(restored).await.expect("destroy restored VM");

    let records = records.lock();
    assert!(!records[before_restore..].is_empty());
    let mut generations = BTreeMap::<u64, BTreeMap<u64, &HarnessEvent>>::new();
    let mut logical = BTreeMap::<u64, &HarnessEvent>::new();
    for (index, (sid, sandbox, event, delivery)) in records.iter().enumerate() {
        assert_eq!(*sid, session);
        let delivery = delivery.expect("SDK events must carry delivery metadata");
        let expected_sandbox = if index < before_restore {
            source
        } else {
            restored
        };
        assert_eq!(
            *sandbox, expected_sandbox,
            "no old connection after restore"
        );
        // ADR 0123 C4: the epoch is the one the SDK held when it sequenced
        // the event. Before the restore everything is epoch 1. After it, a
        // replay of an unacknowledged event keeps epoch 1 and its seq; a new
        // event is epoch 2.
        if index < before_restore {
            assert_eq!(delivery.binding_epoch, 1);
        } else if delivery.binding_epoch == 1 {
            assert!(
                logical.contains_key(&delivery.seq) || !generations.contains_key(&2),
                "an epoch-1 event after the restore must be a replay"
            );
        } else {
            assert_eq!(delivery.binding_epoch, 2);
        }
        if let Some(previous) = generations
            .entry(delivery.binding_epoch)
            .or_default()
            .insert(delivery.seq, event)
        {
            assert_eq!(previous, event, "a duplicate must retain its payload");
        }
        // An ack can be in flight at capture. A replay across the new epoch
        // must retain the process sequence and payload, not create a new event.
        if let Some(previous) = logical.insert(delivery.seq, event) {
            assert_eq!(previous, event, "replay changed the event at this sequence");
        }
    }
    assert_eq!(generations.keys().copied().collect::<Vec<_>>(), [1, 2]);
    // ADR 0123 C5: the re-attached engine names the run it continues under
    // the new generation, exactly once.
    let continued: Vec<_> = generations[&2]
        .values()
        .filter_map(|event| match event {
            HarnessEvent::RunContinued { run_id } => Some(run_id),
            _ => None,
        })
        .collect();
    assert_eq!(continued.len(), 1, "one RunContinued under epoch 2");
    let seqs: Vec<_> = logical.keys().copied().collect();
    assert_eq!(
        seqs,
        (1..=u64::try_from(logical.len()).unwrap()).collect::<Vec<_>>(),
        "gap across restore"
    );
    let events: Vec<_> = logical.values().copied().collect();
    let starts: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::RunStarted {
                run_id, prompt_id, ..
            } => Some((run_id, prompt_id)),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1, "no second RunStarted");
    let (run_id, prompt_id) = starts[0];
    assert_eq!(prompt_id.as_deref(), Some("p1"));
    assert_eq!(continued[0], run_id, "RunContinued names the open run");
    let tool_starts: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::ToolCallStarted {
                run_id: id,
                tool_call_id,
                ..
            } => {
                assert_eq!(id, run_id);
                Some(tool_call_id)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_starts.len(),
        1,
        "no new sequence for a replayed tool start"
    );
    let tools: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(i, event)| match event {
            HarnessEvent::ToolCallCompleted {
                run_id: id,
                tool_call_id,
                ok,
                ..
            } => {
                assert_eq!(tool_call_id, tool_starts[0]);
                assert_eq!(id, run_id);
                assert!(*ok);
                Some(i)
            }
            _ => None,
        })
        .collect();
    assert_eq!(tools.len(), 1);
    let ends: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(i, event)| match event {
            HarnessEvent::RunCompleted { run_id: id, ok } => {
                assert_eq!(id, run_id);
                assert!(*ok);
                Some(i)
            }
            _ => None,
        })
        .collect();
    assert_eq!(ends.len(), 1);
    assert!(tools[0] < ends[0]);
    assert!(matches!(events.get(ends[0] + 1), Some(HarnessEvent::Idle)));
    assert!(events
        .iter()
        .all(|event| !matches!(event, HarnessEvent::RunInterrupted { .. })));
}
