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
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::sandbox::{AgentSpec, ExecEvent, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxError, SandboxId, SessionId};
use futures::Stream;
use std::pin::Pin;
use tonic::transport::Channel;

use crate::grpc::host_service_client::HostServiceClient;
use crate::grpc::{
    ApplyEgressPolicyRequest, BindHarnessSessionRequest, CreateSandboxRequest, Empty,
    ExecStartRequest, GuestIpResponse, ReapMaterializeDirRequest, RestoreRequest, SandboxIdMessage,
    SendHarnessPromptRequest, StartAgentRequest, UnbindHarnessSessionRequest,
};
use crate::wire::{WireExecRequest, WireReapStats};

/// Coord-side client wrapping a `tonic::transport::Channel` to one
/// host. Cheap to clone (tonic clients hold an `Arc<Channel>`
/// internally); one instance per host is the steady-state shape.
#[derive(Clone)]
pub struct GrpcHostClient {
    inner: HostServiceClient<Channel>,
}

impl GrpcHostClient {
    /// Build a client over an existing tonic `Channel`. The channel
    /// is HTTP/2-multiplexing — many concurrent RPCs share one
    /// TCP+H2 connection — so we want exactly one instance per
    /// host. Callers go through `GrpcHostPool::dispatch` which
    /// hands out a clone of the pool's entry.
    pub fn new(channel: Channel) -> Self {
        Self {
            inner: HostServiceClient::new(channel),
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
        let req = RestoreRequest {
            metadata_bincode: encode_bincode(&metadata, "SnapshotMetadata")?,
        };
        let resp = self
            .inner
            .clone()
            .restore(req)
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
        text: String,
    ) -> Result<(), SandboxError> {
        let req = SendHarnessPromptRequest {
            sandbox_id: sandbox_id.as_uuid().as_bytes().to_vec(),
            text,
        };
        self.inner
            .clone()
            .send_harness_prompt(req)
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

    // ---- ADR 0014 warm pool ----

    pub async fn lease_warm_sandbox(
        &self,
        template_ref: engram_core::types::ids::TemplateRef,
    ) -> Result<engram_core::traits::host_client::WarmLeaseOutcome, SandboxError> {
        use crate::grpc::lease_warm_response::Outcome as PbOutcome;
        use crate::grpc::LeaseWarmRequest;
        use engram_core::traits::host_client::WarmLeaseOutcome;

        let req = LeaseWarmRequest {
            template_ref: template_ref.as_uuid().as_bytes().to_vec(),
        };
        let resp = self
            .inner
            .clone()
            .lease_warm_sandbox(req)
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        match resp.outcome {
            Some(PbOutcome::SandboxId(bytes)) => {
                let id = decode_sandbox_id(&bytes)?;
                Ok(WarmLeaseOutcome::Granted(id))
            }
            Some(PbOutcome::Stale(stale)) => {
                let bytes: [u8; 16] = stale.current_ref.as_slice().try_into().map_err(|_| {
                    SandboxError::InvalidSpec(
                        "lease_warm_sandbox stale.current_ref not 16 bytes".into(),
                    )
                })?;
                Ok(WarmLeaseOutcome::Stale {
                    current_ref: engram_core::types::ids::TemplateRef::from(
                        uuid::Uuid::from_bytes(bytes),
                    ),
                })
            }
            Some(PbOutcome::NoCapacity(_)) | None => Ok(WarmLeaseOutcome::NoCapacity),
        }
    }

    pub async fn launch_warm_sandbox(
        &self,
        sandbox_id: SandboxId,
        agent: AgentSpec,
        policy: SessionEgressPolicy,
        harness_pack_uri: Option<String>,
        harness_name: Option<String>,
    ) -> Result<(), SandboxError> {
        use crate::grpc::LaunchWarmRequest;
        let req = LaunchWarmRequest {
            sandbox_id: sandbox_id.as_uuid().as_bytes().to_vec(),
            agent_bincode: encode_bincode(&agent, "AgentSpec")?,
            policy_bincode: encode_bincode(&policy, "SessionEgressPolicy")?,
            // proto3 strings can't be Option; empty == None on the
            // wire. Host's gRPC server maps "" → None before calling
            // the HostClient trait method.
            harness_pack_uri: harness_pack_uri.unwrap_or_default(),
            harness_name: harness_name.unwrap_or_default(),
        };
        self.inner
            .clone()
            .launch_warm_sandbox(req)
            .await
            .map_err(grpc_to_sandbox_err)?;
        Ok(())
    }

    pub async fn list_warm_slots(
        &self,
    ) -> Result<Vec<engram_core::traits::host_client::WarmSlotCount>, SandboxError> {
        use engram_core::traits::host_client::WarmSlotCount;
        let resp = self
            .inner
            .clone()
            .list_warm_slots(Empty {})
            .await
            .map_err(grpc_to_sandbox_err)?
            .into_inner();
        resp.slots
            .into_iter()
            .map(|slot| {
                let bytes: [u8; 16] = slot.template_ref.as_slice().try_into().map_err(|_| {
                    SandboxError::InvalidSpec("WarmSlotCount.template_ref not 16 bytes".into())
                })?;
                Ok(WarmSlotCount {
                    template_ref: engram_core::types::ids::TemplateRef::from(
                        uuid::Uuid::from_bytes(bytes),
                    ),
                    available: slot.available,
                    target: slot.target,
                })
            })
            .collect()
    }
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

    async fn commit_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        Self::commit_snapshot(self, id).await
    }

    async fn abort_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        Self::abort_snapshot(self, id).await
    }

    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        Self::restore(self, metadata).await
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

    async fn send_prompt(&self, sandbox_id: SandboxId, text: String) -> Result<(), SandboxError> {
        self.send_harness_prompt(sandbox_id, text).await
    }

    async fn acquire_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        Self::acquire_shell(self, sandbox_id).await
    }

    async fn release_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        Self::release_shell(self, sandbox_id).await
    }

    async fn lease_warm_sandbox(
        &self,
        template_ref: engram_core::types::ids::TemplateRef,
    ) -> Result<engram_core::traits::host_client::WarmLeaseOutcome, SandboxError> {
        Self::lease_warm_sandbox(self, template_ref).await
    }

    async fn launch_warm_sandbox(
        &self,
        sandbox_id: SandboxId,
        agent: AgentSpec,
        policy: SessionEgressPolicy,
        harness_pack_uri: Option<String>,
        harness_name: Option<String>,
    ) -> Result<(), SandboxError> {
        Self::launch_warm_sandbox(
            self,
            sandbox_id,
            agent,
            policy,
            harness_pack_uri,
            harness_name,
        )
        .await
    }

    async fn list_warm_slots(
        &self,
    ) -> Result<Vec<engram_core::traits::host_client::WarmSlotCount>, SandboxError> {
        Self::list_warm_slots(self).await
    }
    // harness_dial + set_harness_sink use the trait defaults — gRPC
    // doesn't carry static capability or in-proc sink wiring.
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
        // Unavailable = host disconnected mid-call / channel evicted.
        // Surfacing as `Vm` rather than a dedicated variant matches
        // the WS path's `ConnectionError::Closed` mapping.
        _ => SandboxError::Vm(format!("grpc {}: {}", status.code(), status.message()).into()),
    }
}
