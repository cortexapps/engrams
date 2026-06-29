//! `GrpcHostClient` — ADR 0013 coord-side client for one host.
//!
//! Wraps a `tonic::transport::Channel` and turns each `HostService`
//! method into a Rust async fn that takes Rust types (SandboxId,
//! SandboxSpec, etc.) and serializes through bincode where the
//! proto layer carries opaque `bytes` payloads.
//!
//! Implements `engram_core::traits::HostClient` so the coord's
//! existing dispatch path (HostRegistry → Arc<dyn HostClient>)
//! drops a `GrpcHostClient` in where the WS RemoteHostClient used
//! to live. ADR 0013's bundled `start_agent(id, agent, policy)`
//! is one gRPC RPC; `apply_egress_policy` is the no-agent
//! companion.

use async_trait::async_trait;
use bytes::Bytes;
use engram_core::traits::HostClient;
use engram_core::types::cow_state::{CowState, CowStateRecord};
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::sandbox::{
    AgentSpec, AuxRoDrive, ExecEvent, ExecRequest, ExecStream, SandboxSpec,
};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxError, SandboxId, SessionId};
use futures::Stream;
use std::pin::Pin;
use std::time::Duration;
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::Channel;

use crate::grpc::host_service_client::HostServiceClient;
use crate::grpc::proxy_shell_message::Body as ProxyShellBody;
use crate::grpc::{
    AnswerHarnessQuestionRequest, ApplyEgressPolicyRequest, BindHarnessSessionRequest,
    BuildBaseSnapshotRequest, CreateSandboxRequest, DequeueHarnessQueuedPromptRequest,
    EditHarnessQueuedPromptRequest, Empty, ExecStartRequest, GuestIpResponse,
    InterruptHarnessRequest, MigrationExportRef, MigrationFetchRequest, MigrationItem,
    ProxyShellBinary, ProxyShellClose, ProxyShellMessage, ProxyShellOpen, ProxyShellPing,
    ProxyShellPong, ProxyShellText, ProxyTarget, ReapMaterializeDirRequest,
    RehandshakeHarnessRequest, RestoreBaseForSessionRequest, RestoreRequest, SandboxIdMessage,
    SendHarnessPromptRequest, StartAgentRequest, StringList, UnbindHarnessSessionRequest,
};

use crate::wire::{WireExecRequest, WireReapStats};

/// Per-request interceptor that injects coord-side request metadata on
/// every outbound coord→host gRPC call:
///   - the current span's W3C `traceparent`, so the host-agent can
///     stitch its spans onto the coord's trace (ADR 0019); and
///   - the coordinator's [`crate::WIRE_VERSION`] under
///     [`crate::wire::WIRE_VERSION_METADATA_KEY`], so the host-agent can
///     refuse a bincode-skewed request loudly (issue #229) instead of
///     letting it surface as a misleading 400 decode error.
///
/// Zero-sized and `Clone`, so it composes into the tonic client type
/// cleanly. The traceparent leg is a no-op when OTLP is disabled
/// (`current_traceparent` returns `None`); the wire-version leg always
/// fires (the value is a compile-time constant that always parses).
#[derive(Clone, Copy, Default)]
pub struct TraceparentInjector;

impl Interceptor for TraceparentInjector {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(tp) = engram_telemetry::current_traceparent() {
            if let Ok(val) = tp.parse() {
                req.metadata_mut().insert("traceparent", val);
            }
        }
        // Issue #229: stamp our wire version so the host can reject a
        // skewed request at the RPC boundary. A constant u32 always
        // renders to a valid ASCII metadata value.
        if let Ok(val) = crate::WIRE_VERSION.to_string().parse() {
            req.metadata_mut()
                .insert(crate::wire::WIRE_VERSION_METADATA_KEY, val);
        }
        Ok(req)
    }
}

/// Deadline applied (via the gRPC `grpc-timeout` header) to the
/// snapshot-restore RPCs — `restore` (resume) and
/// `restore_base_for_session` (cold-create-via-restore). Without it a
/// host that wedges mid-restore leaves the coord caller hung
/// indefinitely (observed as a ~6-minute dead-host stall); the
/// keepalive pings only catch a *silent* connection, not a peer that
/// ACKs but never completes the call. Set generously above the
/// slowest legitimate restore (cold boot ~15-30s, rechunk-heavy
/// resumes up to a couple of minutes) so it never aborts a real
/// restore, while still bounding the pathological hang. Because it
/// rides the gRPC deadline header, the *host* side observes it too and
/// can abort its own work. Override via
/// `ENGRAM_RESTORE_RPC_TIMEOUT_SECS`.
fn restore_rpc_timeout() -> Duration {
    let secs = std::env::var("ENGRAM_RESTORE_RPC_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(240);
    Duration::from_secs(secs)
}

/// Coord-side client wrapping a `tonic::transport::Channel` to one
/// host. Cheap to clone (tonic clients hold an `Arc<Channel>`
/// internally); one instance per host is the steady-state shape.
#[derive(Clone)]
pub struct GrpcHostClient {
    inner: HostServiceClient<InterceptedService<Channel, TraceparentInjector>>,
}

impl GrpcHostClient {
    /// Build a client over an existing tonic `Channel`. The channel
    /// is HTTP/2-multiplexing — many concurrent RPCs share one
    /// TCP+H2 connection — so we want exactly one instance per
    /// host. Callers go through `GrpcHostPool::dispatch` which
    /// hands out a clone of the pool's entry.
    ///
    /// Every RPC carries the caller's `traceparent` via
    /// [`TraceparentInjector`] (ADR 0019 distributed tracing).
    pub fn new(channel: Channel) -> Self {
        Self {
            inner: HostServiceClient::with_interceptor(channel, TraceparentInjector),
        }
    }

    /// Bump the inbound decode cap for `ReapMaterializeDir` (1M live
    /// disk-manifest UUIDs ≈ 16 MiB on the wire). Other methods stay
    /// at tonic's 4 MiB default; SandboxSpec / SnapshotMetadata are
    /// well under that.
    pub fn with_reap_decode_cap(mut self) -> Self {
        // `max_decoding_message_size` is set per-client; tonic v0.12
        // applies the cap to every response. We only need it for the
        // `Reap` reply path, but the cost of raising it globally on
        // this client is just a header value — no allocation change.
        self.inner = self.inner.max_decoding_message_size(32 * 1024 * 1024);
        self
    }

    /// Fire a no-op `Ping` to force TCP+H2 handshake on a freshly
    /// built `Channel::connect_lazy`. Used by `GrpcHostPool::warm`
    /// so the next real RPC doesn't pay cold-dial latency.
    pub async fn ping(&self) -> Result<(), tonic::Status> {
        self.inner.clone().ping(Empty {}).await?;
        Ok(())
    }

    // ---- unary RPCs returning Rust types ----

    pub async fn create_sandbox(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        let req = CreateSandboxRequest {
            spec_bincode: encode_bincode(&spec, "SandboxSpec")?,
        };
        let resp = self
            .inner
            .clone()
            .create_sandbox(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        decode_sandbox_id(&resp.sandbox_id)
    }

    pub async fn destroy_sandbox(&self, id: SandboxId) -> Result<(), SandboxError> {
        let req = SandboxIdMessage {
            uuid: id.as_uuid().as_bytes().to_vec(),
        };
        self.inner
            .clone()
            .destroy_sandbox(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn list_sandboxes(&self) -> Result<Vec<SandboxId>, SandboxError> {
        let resp = self
            .inner
            .clone()
            .list_sandboxes(Empty {})
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        resp.sandbox_ids
            .iter()
            .map(|b| decode_sandbox_id(b))
            .collect()
    }

    pub async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        let req = SandboxIdMessage {
            uuid: id.as_uuid().as_bytes().to_vec(),
        };
        let resp = self
            .inner
            .clone()
            .snapshot(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        decode_bincode(&resp.metadata_bincode, "SnapshotMetadata")
    }

    /// ADR 0045 D5. An `Unimplemented` status from a pre-D5 host-agent
    /// maps to `InvalidSpec` (same shape as the trait default), which the
    /// coordinator treats as "fall back to the composed snapshot()".
    pub async fn snapshot_begin(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::SnapshotId, SandboxError> {
        let req = SandboxIdMessage {
            uuid: id.as_uuid().as_bytes().to_vec(),
        };
        let resp = self
            .inner
            .clone()
            .snapshot_begin(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        let uuid = uuid::Uuid::from_slice(&resp.snapshot_id)
            .map_err(|e| SandboxError::Snapshot(format!("snapshot_begin id decode: {e}")))?;
        Ok(engram_core::types::SnapshotId::from(uuid))
    }

    /// ADR 0045 D5: await the host-side background upload.
    pub async fn snapshot_wait(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        let req = SandboxIdMessage {
            uuid: id.as_uuid().as_bytes().to_vec(),
        };
        let resp = self
            .inner
            .clone()
            .snapshot_wait(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        decode_bincode(&resp.metadata_bincode, "SnapshotMetadata")
    }

    /// ADR 0045 C1: freeze a sandbox for a live move (coordinator → source).
    pub async fn migration_capture(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::MigrationCaptureOut, SandboxError> {
        let req = SandboxIdMessage {
            uuid: id.as_uuid().as_bytes().to_vec(),
        };
        let resp = self
            .inner
            .clone()
            .migration_capture(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        let to32 = |v: Vec<u8>| -> Result<[u8; 32], SandboxError> {
            v.as_slice()
                .try_into()
                .map_err(|_| SandboxError::Snapshot("chunk hash must be 32 bytes".into()))
        };
        Ok(engram_core::types::snapshot::MigrationCaptureOut {
            export_id: resp.export_id,
            memory_manifest_json: resp.memory_manifest_json,
            disk_manifest_json: resp.disk_manifest_json,
            memory_manifest_ref: decode_bincode(&resp.memory_manifest_ref, "ManifestRef")?,
            disk_manifest_ref: decode_bincode(&resp.disk_manifest_ref, "ManifestRef")?,
            new_memory_chunk_hashes: resp
                .new_memory_chunk_hashes
                .into_iter()
                .map(to32)
                .collect::<Result<_, _>>()?,
            new_disk_chunk_hashes: resp
                .new_disk_chunk_hashes
                .into_iter()
                .map(to32)
                .collect::<Result<_, _>>()?,
            snapshot_id: engram_core::types::SnapshotId::from(
                uuid::Uuid::from_slice(&resp.snapshot_id)
                    .map_err(|e| SandboxError::Snapshot(format!("snapshot id decode: {e}")))?,
            ),
            paused_at_unix_ms: resp.paused_at_unix_ms,
            hot_chunks: resp
                .hot_chunks
                .into_iter()
                .map(to32)
                .collect::<Result<_, _>>()?,
        })
    }

    /// ADR 0045 C2: the pre-pause presetup half of a post-copy move.
    pub async fn migration_presetup(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::MigrationPresetupOut, SandboxError> {
        let req = SandboxIdMessage {
            uuid: id.as_uuid().as_bytes().to_vec(),
        };
        let resp = self
            .inner
            .clone()
            .migration_presetup(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        let to32 = |v: Vec<u8>| -> Result<[u8; 32], SandboxError> {
            v.as_slice()
                .try_into()
                .map_err(|_| SandboxError::Snapshot("chunk hash must be 32 bytes".into()))
        };
        Ok(engram_core::types::snapshot::MigrationPresetupOut {
            export_id: resp.export_id,
            peer_token: resp.peer_token,
            peer_port: resp.peer_port as u16,
            sidecar_json: resp.sidecar_json,
            memory_manifest_json: resp.memory_manifest_json,
            memory_manifest_ref: decode_bincode(&resp.memory_manifest_ref, "ManifestRef")?,
            disk_manifest_ref: decode_bincode(&resp.disk_manifest_ref, "Option<ManifestRef>")?,
            hot_chunks: resp
                .hot_chunks
                .into_iter()
                .map(to32)
                .collect::<Result<_, _>>()?,
        })
    }

    /// ADR 0045 C2: the blackout capture half (pause → vmstate-only →
    /// pagemap seal).
    pub async fn migration_capture_postcopy(
        &self,
        id: SandboxId,
        export_id: &str,
    ) -> Result<engram_core::types::snapshot::PostCopyCaptureOut, SandboxError> {
        let req = MigrationExportRef {
            sandbox_id: id.as_uuid().as_bytes().to_vec(),
            export_id: export_id.to_string(),
        };
        let resp = self
            .inner
            .clone()
            .migration_capture_post_copy(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        Ok(engram_core::types::snapshot::PostCopyCaptureOut {
            sealed_chunks: resp.sealed_chunks,
            total_chunks: resp.total_chunks,
            pause_ms: resp.pause_ms,
            disk_drain_ms: resp.disk_drain_ms,
            vmstate_ms: resp.vmstate_ms,
            scan_ms: resp.scan_ms,
            sealed_disk_chunks: resp.sealed_disk_chunks,
            paused_at_unix_ms: resp.paused_at_unix_ms,
        })
    }

    /// ADR 0045 C2: await the destination's drain outcome.
    pub async fn migration_drain_wait(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::DrainOutcome, SandboxError> {
        let req = SandboxIdMessage {
            uuid: id.as_uuid().as_bytes().to_vec(),
        };
        let resp = self
            .inner
            .clone()
            .migration_drain_wait(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        use engram_core::types::snapshot::DrainOutcome;
        Ok(if resp.done {
            DrainOutcome::Done {
                pulled: resp.pulled,
                alt_sourced: resp.alt_sourced,
                zero_chunks: resp.zero_chunks,
                ms: resp.ms,
            }
        } else {
            DrainOutcome::PeerLost {
                remaining: resp.remaining,
                detail: resp.detail,
            }
        })
    }

    /// ADR 0045 C1: pull an export's artifacts (destination host → source host).
    pub async fn migration_fetch(
        &self,
        export_id: &str,
        items: Vec<engram_core::types::snapshot::MigrationItem>,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<engram_core::types::snapshot::MigrationFrame, SandboxError>,
        >,
        SandboxError,
    > {
        use crate::grpc::migration_item::Kind;
        let wire_items = items
            .into_iter()
            .map(|item| match item {
                engram_core::types::snapshot::MigrationItem::StateBin => MigrationItem {
                    kind: Kind::StateBin as i32,
                    hash: Vec::new(),
                    chunk_idx: 0,
                },
                engram_core::types::snapshot::MigrationItem::Sidecar => MigrationItem {
                    kind: Kind::Sidecar as i32,
                    hash: Vec::new(),
                    chunk_idx: 0,
                },
                engram_core::types::snapshot::MigrationItem::Chunk(h) => MigrationItem {
                    kind: Kind::Chunk as i32,
                    hash: h.to_vec(),
                    chunk_idx: 0,
                },
                engram_core::types::snapshot::MigrationItem::DiskManifest => MigrationItem {
                    kind: Kind::DiskManifest as i32,
                    hash: Vec::new(),
                    chunk_idx: 0,
                },
                engram_core::types::snapshot::MigrationItem::DiskSealInfo => MigrationItem {
                    kind: Kind::DiskSealInfo as i32,
                    hash: Vec::new(),
                    chunk_idx: 0,
                },
                engram_core::types::snapshot::MigrationItem::DiskChunkAt(idx) => MigrationItem {
                    kind: Kind::DiskChunkAt as i32,
                    hash: Vec::new(),
                    chunk_idx: idx,
                },
            })
            .collect();
        let resp = self
            .inner
            .clone()
            .migration_fetch(MigrationFetchRequest {
                export_id: export_id.to_string(),
                items: wire_items,
            })
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        use futures::StreamExt;
        Ok(resp
            .map(|frame| {
                frame
                    .map(|f| engram_core::types::snapshot::MigrationFrame {
                        item_idx: f.item_idx,
                        offset: f.offset,
                        data: bytes::Bytes::from(f.data),
                        last: f.last,
                    })
                    .map_err(grpc_to_sandbox_err)
            })
            .boxed())
    }

    /// ADR 0045 C1.
    pub async fn migration_commit(
        &self,
        id: SandboxId,
        export_id: &str,
    ) -> Result<(), SandboxError> {
        self.inner
            .clone()
            .migration_commit(MigrationExportRef {
                sandbox_id: id.as_uuid().as_bytes().to_vec(),
                export_id: export_id.to_string(),
            })
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    /// ADR 0045 C1.
    pub async fn migration_abort(
        &self,
        id: SandboxId,
        export_id: &str,
    ) -> Result<(), SandboxError> {
        self.inner
            .clone()
            .migration_abort(MigrationExportRef {
                sandbox_id: id.as_uuid().as_bytes().to_vec(),
                export_id: export_id.to_string(),
            })
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn commit_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        let req = SandboxIdMessage {
            uuid: id.as_uuid().as_bytes().to_vec(),
        };
        self.inner
            .clone()
            .commit_snapshot(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn abort_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        let req = SandboxIdMessage {
            uuid: id.as_uuid().as_bytes().to_vec(),
        };
        self.inner
            .clone()
            .abort_snapshot(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        let mut req = tonic::Request::new(RestoreRequest {
            metadata_bincode: encode_bincode(&metadata, "SnapshotMetadata")?,
        });
        req.set_timeout(restore_rpc_timeout());
        let resp = self
            .inner
            .clone()
            .restore(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        decode_sandbox_id(&resp.uuid)
    }

    pub async fn build_base_snapshot(
        &self,
        spec: SandboxSpec,
        warm: Option<engram_core::types::image::WarmConfig>,
        capture_env: std::collections::HashMap<String, String>,
    ) -> Result<SnapshotMetadata, SandboxError> {
        let req = BuildBaseSnapshotRequest {
            spec_bincode: encode_bincode(&spec, "SandboxSpec")?,
            warm_bincode: encode_bincode(&warm, "Option<WarmConfig>")?,
            capture_env_bincode: encode_bincode(&capture_env, "capture_env")?,
        };
        let resp = self
            .inner
            .clone()
            .build_base_snapshot(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        decode_bincode(&resp.metadata_bincode, "SnapshotMetadata")
    }

    pub async fn restore_base_for_session(
        &self,
        metadata: SnapshotMetadata,
        session_env: std::collections::HashMap<String, String>,
        selected_mounts: Vec<AuxRoDrive>,
    ) -> Result<SandboxId, SandboxError> {
        let mut req = tonic::Request::new(RestoreBaseForSessionRequest {
            metadata_bincode: encode_bincode(&metadata, "SnapshotMetadata")?,
            session_env,
            // ADR 0055: per-session selected skills, assigned to reserved slots.
            selected_mounts_bincode: encode_bincode(&selected_mounts, "selected_mounts")?,
        });
        req.set_timeout(restore_rpc_timeout());
        let resp = self
            .inner
            .clone()
            .restore_base_for_session(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        decode_sandbox_id(&resp.uuid)
    }

    pub async fn guest_ip(&self, id: SandboxId) -> Option<String> {
        let req = SandboxIdMessage {
            uuid: id.as_uuid().as_bytes().to_vec(),
        };
        let resp: GuestIpResponse = self.inner.clone().guest_ip(req).await.ok()?.into_inner();
        resp.ip
    }

    pub async fn bind_harness_session(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
    ) -> Result<(), SandboxError> {
        let req = BindHarnessSessionRequest {
            session_id: session_id.as_uuid().as_bytes().to_vec(),
            sandbox_id: sandbox_id.as_uuid().as_bytes().to_vec(),
        };
        self.inner
            .clone()
            .bind_harness_session(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn unbind_harness_session(&self, session_id: SessionId) -> Result<(), SandboxError> {
        let req = UnbindHarnessSessionRequest {
            session_id: session_id.as_uuid().as_bytes().to_vec(),
        };
        self.inner
            .clone()
            .unbind_harness_session(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn send_harness_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
    ) -> Result<(), SandboxError> {
        let req = SendHarnessPromptRequest {
            sandbox_id: sandbox_id.as_uuid().as_bytes().to_vec(),
            text,
            prompt_id,
        };
        self.inner
            .clone()
            .send_harness_prompt(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn edit_harness_queued_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
    ) -> Result<(), SandboxError> {
        let req = EditHarnessQueuedPromptRequest {
            sandbox_id: sandbox_id.as_uuid().as_bytes().to_vec(),
            prompt_id,
            text,
        };
        self.inner
            .clone()
            .edit_harness_queued_prompt(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn dequeue_harness_queued_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
    ) -> Result<(), SandboxError> {
        let req = DequeueHarnessQueuedPromptRequest {
            sandbox_id: sandbox_id.as_uuid().as_bytes().to_vec(),
            prompt_id,
        };
        self.inner
            .clone()
            .dequeue_harness_queued_prompt(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn answer_harness_question(
        &self,
        sandbox_id: SandboxId,
        tool_call_id: String,
        answers: std::collections::BTreeMap<String, Vec<String>>,
    ) -> Result<(), SandboxError> {
        let req = AnswerHarnessQuestionRequest {
            sandbox_id: sandbox_id.as_uuid().as_bytes().to_vec(),
            tool_call_id,
            // Canonical Answers → proto map<string, StringList>.
            answers: answers
                .into_iter()
                .map(|(question, values)| (question, StringList { values }))
                .collect(),
        };
        self.inner
            .clone()
            .answer_harness_question(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn interrupt_harness(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let req = InterruptHarnessRequest {
            sandbox_id: sandbox_id.as_uuid().as_bytes().to_vec(),
        };
        self.inner
            .clone()
            .interrupt_harness(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn rehandshake_harness(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let req = RehandshakeHarnessRequest {
            sandbox_id: sandbox_id.as_uuid().as_bytes().to_vec(),
        };
        self.inner
            .clone()
            .rehandshake_harness(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    /// ADR 0045 Phase F: freeze the microVM in place.
    pub async fn pause_sandbox(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let req = SandboxIdMessage {
            uuid: sandbox_id.as_uuid().as_bytes().to_vec(),
        };
        self.inner
            .clone()
            .pause_sandbox(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    /// ADR 0045 Phase F: unfreeze a paused microVM.
    pub async fn resume_sandbox(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let req = SandboxIdMessage {
            uuid: sandbox_id.as_uuid().as_bytes().to_vec(),
        };
        self.inner
            .clone()
            .resume_sandbox(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    /// Bundled `start_agent` (ADR 0013 atomicity): the host applies
    /// the egress policy to its proxy registry BEFORE spawning the
    /// agent process. Caller must always pass a policy — the
    /// no-agent / no-policy case routes through `apply_egress_policy`
    /// instead.
    pub async fn start_agent(
        &self,
        sandbox_id: SandboxId,
        agent: AgentSpec,
        policy: SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        let req = StartAgentRequest {
            sandbox_id: sandbox_id.as_uuid().as_bytes().to_vec(),
            agent_bincode: encode_bincode(&agent, "AgentSpec")?,
            policy_bincode: encode_bincode(&policy, "SessionEgressPolicy")?,
        };
        self.inner
            .clone()
            .start_agent(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    /// Apply a SessionEgressPolicy without spawning an agent. The
    /// companion to `start_agent`'s bundled form for no-agent
    /// sessions.
    pub async fn apply_egress_policy(
        &self,
        policy: SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        let req = ApplyEgressPolicyRequest {
            policy_bincode: encode_bincode(&policy, "SessionEgressPolicy")?,
        };
        self.inner
            .clone()
            .apply_egress_policy(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn acquire_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let req = SandboxIdMessage {
            uuid: sandbox_id.as_uuid().as_bytes().to_vec(),
        };
        self.inner
            .clone()
            .acquire_shell(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn release_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let req = SandboxIdMessage {
            uuid: sandbox_id.as_uuid().as_bytes().to_vec(),
        };
        self.inner
            .clone()
            .release_shell(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    /// Issue #219: refresh a live shell pin's keep-alive stamp.
    pub async fn renew_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let req = SandboxIdMessage {
            uuid: sandbox_id.as_uuid().as_bytes().to_vec(),
        };
        self.inner
            .clone()
            .renew_shell(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    /// ADR 0016 Phase A: per-sandbox COW diagnostic snapshot. Empty
    /// `state_bincode` on the wire encodes `None` (host has no
    /// chunk-tracked view of this sandbox) so coord can distinguish
    /// "not chunk-tracked" from "really nothing dirty".
    pub async fn cow_state(&self, id: SandboxId) -> Result<Option<CowState>, SandboxError> {
        let req = SandboxIdMessage {
            uuid: id.as_uuid().as_bytes().to_vec(),
        };
        let resp = self
            .inner
            .clone()
            .cow_state(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        if resp.state_bincode.is_empty() {
            return Ok(None);
        }
        Ok(Some(decode_bincode(&resp.state_bincode, "CowState")?))
    }

    /// ADR 0016 Phase A: bulk COW fetch for one host. Returns one
    /// record per chunk-tracked sandbox; ordering not guaranteed.
    pub async fn cow_state_all(&self) -> Result<Vec<CowStateRecord>, SandboxError> {
        let resp = self
            .inner
            .clone()
            .cow_state_all(Empty {})
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        decode_bincode(&resp.records_bincode, "Vec<CowStateRecord>")
    }

    /// ADR 0016 Phase B commit 4a — admin flush trigger. Coord's
    /// `POST /api/admin/sessions/:id/flush-now` calls this. Empty
    /// `manifest_ref_bincode` on the wire encodes `None` (no chunk
    /// tracking / no dirty chunks); non-empty decodes to the new
    /// `Some(ManifestRef)`.
    pub async fn flush_sandbox(
        &self,
        id: SandboxId,
    ) -> Result<Option<engram_core::types::manifest::ManifestRef>, SandboxError> {
        let req = SandboxIdMessage {
            uuid: id.as_uuid().as_bytes().to_vec(),
        };
        let resp = self
            .inner
            .clone()
            .flush_sandbox(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        if resp.manifest_ref_bincode.is_empty() {
            return Ok(None);
        }
        Ok(Some(decode_bincode(
            &resp.manifest_ref_bincode,
            "ManifestRef",
        )?))
    }

    pub async fn reap_materialize_dir(
        &self,
        min_age_secs: u64,
        live_disk_manifest_ids: Vec<uuid::Uuid>,
    ) -> Result<WireReapStats, SandboxError> {
        let req = ReapMaterializeDirRequest {
            min_age_secs,
            live_disk_manifest_ids: live_disk_manifest_ids
                .into_iter()
                .map(|u| u.as_bytes().to_vec())
                .collect(),
        };
        let resp = self
            .inner
            .clone()
            .reap_materialize_dir(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        decode_bincode(&resp.stats_bincode, "WireReapStats")
    }

    /// Server-streaming exec. The first frame is `started` (carries
    /// the host-assigned `exec_id`); subsequent frames carry
    /// stdout/stderr bytes; the stream ends with exactly one `exit`
    /// frame. Returned `ExecStream` mirrors the same shape the WS
    /// path returns so coord-side consumers don't notice.
    pub async fn exec_start(
        &self,
        sandbox_id: SandboxId,
        request: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        let wire = WireExecRequest::from_engine(request);
        let req = ExecStartRequest {
            sandbox_id: sandbox_id.as_uuid().as_bytes().to_vec(),
            request_bincode: encode_bincode(&wire, "WireExecRequest")?,
        };
        let mut stream = self
            .inner
            .clone()
            .exec_start(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();

        // First frame must be `started` so we know the exec_id. Pull
        // it synchronously before handing the rest of the stream
        // back to the caller.
        let started = stream
            .message()
            .await
            .map_err(grpc_to_sandbox_err)?
            .ok_or_else(|| {
                SandboxError::Vm("exec_start gRPC stream closed before `started` frame".into())
            })?;
        let exec_id = match started.frame {
            Some(crate::grpc::exec_frame::Frame::Started(id)) => id,
            other => {
                return Err(SandboxError::Vm(
                    format!("exec_start: expected Started frame, got {other:?}").into(),
                ));
            }
        };

        let events = async_stream::stream! {
            loop {
                match stream.message().await {
                    Ok(Some(frame)) => match frame.frame {
                        Some(crate::grpc::exec_frame::Frame::Stdout(b)) => {
                            yield ExecEvent::Stdout(Bytes::from(b));
                        }
                        Some(crate::grpc::exec_frame::Frame::Stderr(b)) => {
                            yield ExecEvent::Stderr(Bytes::from(b));
                        }
                        Some(crate::grpc::exec_frame::Frame::Exit(exit)) => {
                            yield ExecEvent::Exit(exit.status);
                            break;
                        }
                        // Spurious Started or an unrecognised oneof
                        // variant — drop and let the stream end.
                        Some(crate::grpc::exec_frame::Frame::Started(_)) | None => {
                            tracing::warn!(
                                "exec_start: unexpected frame variant after Started; dropping"
                            );
                        }
                    },
                    Ok(None) => break,
                    Err(e) => {
                        // Surface gRPC-level errors as an Exit(None)
                        // so the demuxer downstream still terminates
                        // cleanly. The error is logged here so it's
                        // visible even if the consumer drops the
                        // stream early.
                        tracing::warn!(error = %e, "exec_start stream error; emitting Exit(None)");
                        yield ExecEvent::Exit(None);
                        break;
                    }
                }
            }
        };

        Ok(ExecStream {
            sandbox_id,
            exec_id,
            events: Box::pin(events) as Pin<Box<dyn Stream<Item = ExecEvent> + Send + 'static>>,
        })
    }

    /// ADR 0014 issue #6: open a bidi ProxyShell stream to the host.
    /// Sends the initial frame carrying `sandbox_id`, then returns a
    /// `ShellTunnel` whose channels the caller bridges to the
    /// browser-side Axum WebSocket.
    pub async fn proxy_shell(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<engram_core::types::shell::ShellTunnel, SandboxError> {
        use engram_core::types::shell::ShellTunnel;
        use futures::StreamExt;

        let (tunnel, ends) = ShellTunnel::pair();
        let engram_core::types::shell::ShellTunnelEnds {
            mut outbound_rx,
            inbound_tx,
        } = ends;

        // First message on the stream is always `Open` — that's the
        // *only* place sandbox_id lives now. Subsequent messages emit
        // pure data variants. There's no way for the host to mistake
        // the sentinel for a data frame because they're distinct enum
        // arms in the prost-generated `ProxyShellBody`.
        let sandbox_bytes = sandbox_id.as_uuid().as_bytes().to_vec();

        let out_stream = async_stream::stream! {
            yield ProxyShellMessage {
                body: Some(ProxyShellBody::Open(ProxyShellOpen {
                    sandbox_id: sandbox_bytes,
                    // ADR 0064: existing shell relay path; default discriminant.
                    // P1.4 threads a real target through for the VNC stream.
                    target: ProxyTarget::Shell as i32,
                })),
            };
            while let Some(frame) = outbound_rx.recv().await {
                yield ProxyShellMessage {
                    body: Some(shell_frame_to_proxy_body(frame)),
                };
            }
        };

        let mut inbound_stream = self
            .inner
            .clone()
            .proxy_shell(out_stream)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();

        // Pump inbound (host → us) into the tunnel's inbound channel.
        tokio::spawn(async move {
            while let Some(next) = inbound_stream.next().await {
                let msg = match next {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(error = %e, "proxy_shell client recv error");
                        break;
                    }
                };
                let sf = match proxy_body_to_shell_frame(msg.body) {
                    Ok(Some(sf)) => sf,
                    Ok(None) => continue, // body=None or Open echoed back (server bug; drop)
                    Err(e) => {
                        tracing::warn!(error = %e, "proxy_shell decode error");
                        break;
                    }
                };
                if inbound_tx.send(sf).await.is_err() {
                    break;
                }
            }
        });

        Ok(tunnel)
    }
}

/// Translate a [`ShellFrame`] into the corresponding `ProxyShellBody`
/// variant. Total — there's no `None` outcome — because every
/// `ShellFrame` variant maps 1:1 to a proxy variant.
fn shell_frame_to_proxy_body(frame: engram_core::types::shell::ShellFrame) -> ProxyShellBody {
    use engram_core::types::shell::ShellFrame;
    match frame {
        ShellFrame::Text(t) => ProxyShellBody::Text(ProxyShellText { data: t }),
        ShellFrame::Binary(b) => ProxyShellBody::Binary(ProxyShellBinary { data: b.to_vec() }),
        ShellFrame::Ping(b) => ProxyShellBody::Ping(ProxyShellPing { data: b.to_vec() }),
        ShellFrame::Pong(b) => ProxyShellBody::Pong(ProxyShellPong { data: b.to_vec() }),
        ShellFrame::Close(None) => ProxyShellBody::Close(ProxyShellClose {
            code: 0,
            reason: String::new(),
        }),
        ShellFrame::Close(Some(c)) => ProxyShellBody::Close(ProxyShellClose {
            code: c.code as u32,
            reason: c.reason,
        }),
    }
}

/// Translate a wire `ProxyShellBody` back into a [`ShellFrame`].
/// Returns:
/// - `Ok(Some(frame))` for the normal data variants.
/// - `Ok(None)` for `body == None` (empty message — prost permits;
///   we ignore) or for an `Open` echoed back from the server (would
///   be a server-side bug; we drop without erroring rather than
///   tear down the entire shell session).
/// - `Err(_)` for genuinely malformed input — currently unreachable
///   given the oneof, kept as a seam if we add fallible variants
///   later (e.g. UTF-8 strict decoding).
fn proxy_body_to_shell_frame(
    body: Option<ProxyShellBody>,
) -> Result<Option<engram_core::types::shell::ShellFrame>, String> {
    use engram_core::types::shell::{ShellClose, ShellFrame};
    Ok(match body {
        None => None,
        Some(ProxyShellBody::Open(_)) => None,
        Some(ProxyShellBody::Text(t)) => Some(ShellFrame::Text(t.data)),
        Some(ProxyShellBody::Binary(b)) => Some(ShellFrame::Binary(b.data.into())),
        Some(ProxyShellBody::Ping(p)) => Some(ShellFrame::Ping(p.data.into())),
        Some(ProxyShellBody::Pong(p)) => Some(ShellFrame::Pong(p.data.into())),
        Some(ProxyShellBody::Close(c)) => {
            if c.code == 0 && c.reason.is_empty() {
                Some(ShellFrame::Close(None))
            } else {
                Some(ShellFrame::Close(Some(ShellClose {
                    code: c.code as u16,
                    reason: c.reason,
                })))
            }
        }
    })
}

// ---- helpers ----
//
// Use `bincode::serialize` / `bincode::deserialize` with default
// config to match the existing WS codec (`src/codec.rs:49,71`) so
// a SandboxSpec encoded on the gRPC wire decodes identically to one
// on the WS wire. Both transports use the same Rust types during
// the ADR 0013 transition.

fn encode_bincode<T: serde::Serialize>(
    value: &T,
    kind: &'static str,
) -> Result<Vec<u8>, SandboxError> {
    bincode::serialize(value)
        .map_err(|e| SandboxError::InvalidSpec(format!("bincode encode {kind}: {e}")))
}

fn decode_bincode<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    kind: &'static str,
) -> Result<T, SandboxError> {
    bincode::deserialize(bytes)
        .map_err(|e| SandboxError::InvalidSpec(format!("bincode decode {kind}: {e}")))
}

fn decode_sandbox_id(bytes: &[u8]) -> Result<SandboxId, SandboxError> {
    if bytes.len() != 16 {
        return Err(SandboxError::InvalidSpec(format!(
            "sandbox_id must be 16 bytes, got {}",
            bytes.len()
        )));
    }
    let mut buf = [0u8; 16];
    buf.copy_from_slice(bytes);
    Ok(SandboxId(uuid::Uuid::from_bytes(buf)))
}

// ADR 0013: trait impl. The coord's `HostRegistry` stores
// `Arc<dyn HostClient>` per host — wrapping a `GrpcHostClient` in
// the trait object lets the existing dispatch machinery route to
// it transparently. Inherent methods do the work; this is pure
// delegation.
#[async_trait]
impl HostClient for GrpcHostClient {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        self.create_sandbox(spec).await
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.destroy_sandbox(id).await
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        self.list_sandboxes().await
    }

    async fn ping(&self) -> Result<(), SandboxError> {
        // Override the trait default (which round-trips `list()`) with
        // the dedicated no-op `Ping` RPC — cheapest possible liveness
        // probe for the dead-host detector's defense-in-depth check
        // (issue #231).
        GrpcHostClient::ping(self)
            .await
            .map_err(|s| SandboxError::Unavailable(format!("ping: {s}")))
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        self.exec_start(id, cmd).await
    }

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        // Disambiguates from the trait's `snapshot` method.
        Self::snapshot(self, id).await
    }

    async fn snapshot_begin(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::SnapshotId, SandboxError> {
        Self::snapshot_begin(self, id).await
    }

    async fn snapshot_wait(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        Self::snapshot_wait(self, id).await
    }

    async fn migration_capture(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::MigrationCaptureOut, SandboxError> {
        Self::migration_capture(self, id).await
    }

    async fn migration_presetup(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::MigrationPresetupOut, SandboxError> {
        Self::migration_presetup(self, id).await
    }

    async fn migration_capture_postcopy(
        &self,
        id: SandboxId,
        export_id: &str,
    ) -> Result<engram_core::types::snapshot::PostCopyCaptureOut, SandboxError> {
        Self::migration_capture_postcopy(self, id, export_id).await
    }

    async fn migration_drain_wait(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::DrainOutcome, SandboxError> {
        Self::migration_drain_wait(self, id).await
    }

    async fn migration_fetch(
        &self,
        export_id: &str,
        items: Vec<engram_core::types::snapshot::MigrationItem>,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<engram_core::types::snapshot::MigrationFrame, SandboxError>,
        >,
        SandboxError,
    > {
        Self::migration_fetch(self, export_id, items).await
    }

    async fn migration_commit(&self, id: SandboxId, export_id: &str) -> Result<(), SandboxError> {
        Self::migration_commit(self, id, export_id).await
    }

    async fn migration_abort(&self, id: SandboxId, export_id: &str) -> Result<(), SandboxError> {
        Self::migration_abort(self, id, export_id).await
    }

    async fn commit_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        Self::commit_snapshot(self, id).await
    }

    async fn abort_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        Self::abort_snapshot(self, id).await
    }

    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        Self::restore(self, metadata).await
    }

    async fn build_base_snapshot(
        &self,
        spec: SandboxSpec,
        warm: Option<engram_core::types::image::WarmConfig>,
        capture_env: std::collections::HashMap<String, String>,
    ) -> Result<SnapshotMetadata, SandboxError> {
        Self::build_base_snapshot(self, spec, warm, capture_env).await
    }

    async fn restore_base_for_session(
        &self,
        metadata: SnapshotMetadata,
        session_env: std::collections::HashMap<String, String>,
        selected_mounts: Vec<AuxRoDrive>,
    ) -> Result<SandboxId, SandboxError> {
        Self::restore_base_for_session(self, metadata, session_env, selected_mounts).await
    }

    async fn start_agent(
        &self,
        id: SandboxId,
        agent: AgentSpec,
        policy: SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        Self::start_agent(self, id, agent, policy).await
    }

    async fn apply_egress_policy(&self, policy: SessionEgressPolicy) -> Result<(), SandboxError> {
        Self::apply_egress_policy(self, policy).await
    }

    async fn guest_ip(&self, id: SandboxId) -> Option<String> {
        Self::guest_ip(self, id).await
    }

    async fn bind_session(&self, session_id: SessionId, sandbox_id: SandboxId) {
        if let Err(e) = self.bind_harness_session(session_id, sandbox_id).await {
            tracing::warn!(%session_id, %sandbox_id, error = %e, "gRPC bind_harness_session failed");
        }
    }

    async fn unbind_session(&self, session_id: SessionId) {
        if let Err(e) = self.unbind_harness_session(session_id).await {
            tracing::warn!(%session_id, error = %e, "gRPC unbind_harness_session failed");
        }
    }

    async fn send_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
    ) -> Result<(), SandboxError> {
        self.send_harness_prompt(sandbox_id, prompt_id, text).await
    }

    async fn edit_queued_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
    ) -> Result<(), SandboxError> {
        self.edit_harness_queued_prompt(sandbox_id, prompt_id, text)
            .await
    }

    async fn dequeue_queued_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
    ) -> Result<(), SandboxError> {
        self.dequeue_harness_queued_prompt(sandbox_id, prompt_id)
            .await
    }

    async fn answer_question(
        &self,
        sandbox_id: SandboxId,
        tool_call_id: String,
        answers: std::collections::BTreeMap<String, Vec<String>>,
    ) -> Result<(), SandboxError> {
        self.answer_harness_question(sandbox_id, tool_call_id, answers)
            .await
    }

    async fn interrupt(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.interrupt_harness(sandbox_id).await
    }

    async fn rehandshake(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.rehandshake_harness(sandbox_id).await
    }

    async fn pause(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.pause_sandbox(sandbox_id).await
    }

    async fn resume(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.resume_sandbox(sandbox_id).await
    }

    async fn acquire_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        Self::acquire_shell(self, sandbox_id).await
    }

    async fn release_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        Self::release_shell(self, sandbox_id).await
    }

    async fn renew_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        Self::renew_shell(self, sandbox_id).await
    }

    async fn proxy_shell(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<engram_core::types::shell::ShellTunnel, SandboxError> {
        Self::proxy_shell(self, sandbox_id).await
    }

    // harness_dial + set_harness_sink use the trait defaults — gRPC
    // doesn't carry static capability or in-proc sink wiring.

    async fn cow_state(&self, id: SandboxId) -> Result<Option<CowState>, SandboxError> {
        Self::cow_state(self, id).await
    }

    async fn cow_state_all(&self) -> Result<Vec<CowStateRecord>, SandboxError> {
        Self::cow_state_all(self).await
    }

    async fn flush_sandbox(
        &self,
        id: SandboxId,
    ) -> Result<Option<engram_core::types::manifest::ManifestRef>, SandboxError> {
        Self::flush_sandbox(self, id).await
    }
}

/// Map a tonic Status into the same `SandboxError` shape today's WS
/// path produces. The bincode wire used `RemoteError::into_sandbox`;
/// tonic gives us a status code + message, so we do the equivalent
/// mapping per code.
fn grpc_to_sandbox_err(status: tonic::Status) -> SandboxError {
    use tonic::Code;
    match status.code() {
        Code::NotFound => SandboxError::NotFound,
        Code::AlreadyExists => SandboxError::AlreadyExists,
        Code::DeadlineExceeded | Code::Aborted => SandboxError::Timeout,
        Code::ResourceExhausted => SandboxError::LimitExceeded(status.message().to_string()),
        Code::InvalidArgument => SandboxError::InvalidSpec(status.message().to_string()),
        // ADR 0045 D5: a pre-D5 host-agent answers the new
        // SnapshotBegin/SnapshotWait RPCs with Unimplemented; map to the
        // same InvalidSpec shape the trait defaults use so the
        // coordinator's "unsupported -> composed snapshot()" fallback
        // fires uniformly for old binaries and non-FC backends alike.
        Code::Unimplemented => SandboxError::InvalidSpec(status.message().to_string()),
        // ADR 0050 C: Unavailable = lazy connect failed / host
        // disconnected mid-call / channel evicted — TRANSIENT. Map to
        // the retryable `Unavailable` variant so the exec/destroy call
        // sites can retry (the pool defers per-RPC retry to them), and
        // the API surfaces a 503, not a 500.
        Code::Unavailable => SandboxError::Unavailable(status.message().to_string()),
        // Issue #229: the host rejected a wire_version-skewed request at
        // the RPC boundary (a `failed_precondition` carrying the skew
        // marker). Map it to the typed, RETRYABLE `WireSkew` variant so
        // the API surfaces a 503 — never the 400 a raw bincode decode
        // error would have produced. A `failed_precondition` WITHOUT the
        // marker (none is emitted host-side today, but be defensive)
        // falls through to the generic mapping.
        Code::FailedPrecondition => match crate::wire::parse_wire_skew_message(status.message()) {
            Some((host, coord)) => SandboxError::WireSkew { host, coord },
            None => {
                SandboxError::Vm(format!("grpc {}: {}", status.code(), status.message()).into())
            }
        },
        _ => SandboxError::Vm(format!("grpc {}: {}", status.code(), status.message()).into()),
    }
}

#[cfg(test)]
mod grpc_err_tests {
    use super::*;

    #[test]
    fn unavailable_maps_to_retryable_variant_not_vm() {
        // ADR 0050 C: tonic `Unavailable` (lazy connect failed / channel
        // evicted) must surface as the retryable `Unavailable` variant so
        // the exec/destroy call sites retry it — NOT collapse into `Vm`
        // (a real VM error the caller would 500 on).
        let err = grpc_to_sandbox_err(tonic::Status::unavailable("tcp connect error"));
        match err {
            SandboxError::Unavailable(msg) => assert!(msg.contains("tcp connect error")),
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn real_vm_error_still_maps_to_vm() {
        let err = grpc_to_sandbox_err(tonic::Status::internal("firecracker panicked"));
        assert!(matches!(err, SandboxError::Vm(_)));
    }

    #[test]
    fn wire_skew_failed_precondition_maps_to_typed_wire_skew_not_invalid_spec() {
        // Issue #229: a host that refused a wire_version-skewed request
        // returns `failed_precondition` carrying the skew marker. It MUST
        // map to the typed, retryable `WireSkew` variant (→ 503), never to
        // `InvalidSpec` (→ the user-facing 400 "invalid sandbox spec" that
        // was the bug) nor to a generic `Vm` (→ 500).
        let status = tonic::Status::failed_precondition(crate::wire::wire_skew_message(2, 3));
        match grpc_to_sandbox_err(status) {
            SandboxError::WireSkew { host, coord } => {
                assert_eq!((host, coord), (2, 3));
            }
            other => panic!("expected WireSkew, got {other:?}"),
        }
    }

    #[test]
    fn bincode_decode_invalid_argument_still_maps_to_invalid_spec() {
        // Guard the contrast: a genuine `invalid_argument` (the shape a raw
        // bincode decode error takes) still maps to `InvalidSpec`. Only the
        // pre-decode skew refusal (`failed_precondition` + marker) is
        // rescued from the 400 path.
        let status =
            tonic::Status::invalid_argument("bincode decode SandboxSpec: unexpected end of file");
        assert!(matches!(
            grpc_to_sandbox_err(status),
            SandboxError::InvalidSpec(_)
        ));
    }

    #[test]
    fn unmarked_failed_precondition_falls_through_to_vm() {
        // A `failed_precondition` WITHOUT the skew marker isn't a version
        // mismatch — it must not be misread as `WireSkew`.
        let err = grpc_to_sandbox_err(tonic::Status::failed_precondition(
            "some other precondition",
        ));
        assert!(matches!(err, SandboxError::Vm(_)));
    }
}
