//! The coord↔host boundary trait.
//!
//! `SandboxBackend` is the VMM dimension (FC vs VZ vs Process — "what
//! kind of VM am I"). `HostClient` is the transport dimension ("where
//! am I"). Splitting the two means the coord never imports
//! `SandboxBackend` directly — it talks to a `HostClient`, which
//! either composes a local backend (in-process) or sends RPCs over the
//! WS (remote). Two impls today:
//!
//! - `LocalHostClient` in `engram-host-agent::host_client` — wraps an
//!   `Arc<dyn SandboxBackend>` + an `Arc<HarnessHub>`. Used in
//!   `--mode=all` (coord constructs one inline) and inside the
//!   host-agent itself (what its WS server serves over the wire).
//! - `RemoteHostClient` in `engram-protocol::client` — wraps a
//!   `ConnectedHost` (WS). Every method is a unary RPC.
//!
//! The trait surface unions what used to live separately on
//! `SandboxBackend`'s coord-facing methods and on `HarnessHub`'s
//! direct in-process API. `snapshot_path_for` and `set_harness_sink`
//! stay on `SandboxBackend` only — they're host-local concepts that
//! don't cross the wire.

use async_trait::async_trait;

use crate::error::SandboxError;
use crate::traits::sandbox::{ForgeSink, HarnessDial, HarnessSink, UploadSink};
use crate::types::cow_state::{CowState, CowStateRecord};
use crate::types::egress::SessionEgressPolicy;
use crate::types::image::WarmConfig;
use crate::types::port::PortTunnel;
use crate::types::sandbox::{AgentSpec, ExecHandle, ExecRequest, ExecStream, SandboxSpec};
use crate::types::shell::ShellTunnel;
use crate::types::snapshot::SnapshotMetadata;
use crate::types::{SandboxId, SessionId};

#[async_trait]
pub trait HostClient: Send + Sync {
    // ---- sandbox lifecycle ----
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError>;
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError>;
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError>;

    /// Cheap liveness probe. Returns `Ok(())` if the host answers an
    /// RPC, `Err` if it's unreachable. Used by the dead-host detector
    /// as a defense-in-depth check before evicting a host that merely
    /// *looks* stale in Postgres (issue #231: an asymmetric PG failure
    /// — one coord pod's pool saturated while a sibling's detector is
    /// healthy — can stale a host's `last_heartbeat_at` row while the
    /// host is alive and serving). The default reuses `list()`, which
    /// every transport already implements and which the heartbeat
    /// reconcile path already round-trips; the gRPC transport overrides
    /// it with the no-op `Ping` RPC.
    async fn ping(&self) -> Result<(), SandboxError> {
        self.list().await.map(|_| ())
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError>;
    async fn exec(&self, id: SandboxId, cmd: ExecRequest) -> Result<ExecHandle, SandboxError> {
        use futures::stream::StreamExt;
        let mut stream = self.exec_stream(id, cmd).await?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_status = None;
        while let Some(event) = stream.events.next().await {
            match event {
                crate::types::sandbox::ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
                crate::types::sandbox::ExecEvent::Stderr(b) => stderr.extend_from_slice(&b),
                crate::types::sandbox::ExecEvent::Exit(s) => {
                    exit_status = s;
                    break;
                }
            }
        }
        Ok(ExecHandle {
            sandbox_id: stream.sandbox_id,
            exec_id: stream.exec_id,
            stdout,
            stderr,
            exit_status,
        })
    }

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError>;
    /// ADR 0045 D5: the pause-side half of an eviction snapshot — pause +
    /// drain + FC capture, then the guest is re-paused (it's being torn
    /// down; today's pipeline already discards post-capture execution).
    /// The chunk+upload work runs as a host-side background task; await it via
    /// [`Self::snapshot_wait`]. Returns the new snapshot's id once the
    /// capture itself has succeeded — the point where the coordinator
    /// may mark the session Idle. Default errs so non-FC hosts and
    /// pre-D5 host-agents fall back to the composed [`Self::snapshot`].
    async fn snapshot_begin(
        &self,
        _id: SandboxId,
    ) -> Result<crate::types::SnapshotId, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this host doesn't support `snapshot_begin`".into(),
        ))
    }
    /// ADR 0045 D5: await the background upload spawned by
    /// [`Self::snapshot_begin`] and return the durable
    /// [`SnapshotMetadata`]. Idempotent w.r.t. reconnects — the upload
    /// is host-autonomous once begun.
    async fn snapshot_wait(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this host doesn't support `snapshot_wait`".into(),
        ))
    }

    /// ADR 0045 C1: see `SandboxBackend::migration_capture`. Default
    /// errs so old hosts route the coordinator to snapshot-rehome.
    async fn migration_capture(
        &self,
        _id: SandboxId,
    ) -> Result<crate::types::snapshot::MigrationCaptureOut, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this host doesn't support `migration_capture`".into(),
        ))
    }

    /// ADR 0045 C1: see `SandboxBackend::migration_fetch`. Called by
    /// the DESTINATION host-agent (the one host-to-host RPC).
    async fn migration_fetch(
        &self,
        _export_id: &str,
        _items: Vec<crate::types::snapshot::MigrationItem>,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<crate::types::snapshot::MigrationFrame, SandboxError>,
        >,
        SandboxError,
    > {
        Err(SandboxError::InvalidSpec(
            "this host doesn't support `migration_fetch`".into(),
        ))
    }

    /// ADR 0045 C2: see `SandboxBackend::migration_presetup`.
    async fn migration_presetup(
        &self,
        _id: SandboxId,
    ) -> Result<crate::types::snapshot::MigrationPresetupOut, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this host doesn't support `migration_presetup`".into(),
        ))
    }

    /// ADR 0045 C2: see `SandboxBackend::migration_capture_postcopy`.
    async fn migration_capture_postcopy(
        &self,
        _id: SandboxId,
        _export_id: &str,
    ) -> Result<crate::types::snapshot::PostCopyCaptureOut, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this host doesn't support `migration_capture_postcopy`".into(),
        ))
    }

    /// ADR 0045 C2: see `SandboxBackend::migration_drain_wait`.
    async fn migration_drain_wait(
        &self,
        _id: SandboxId,
    ) -> Result<crate::types::snapshot::DrainOutcome, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this host doesn't support `migration_drain_wait`".into(),
        ))
    }

    /// ADR 0045 C1: see `SandboxBackend::migration_commit`.
    async fn migration_commit(&self, _id: SandboxId, _export_id: &str) -> Result<(), SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this host doesn't support `migration_commit`".into(),
        ))
    }

    /// ADR 0045 C1: see `SandboxBackend::migration_abort`.
    async fn migration_abort(&self, _id: SandboxId, _export_id: &str) -> Result<(), SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this host doesn't support `migration_abort`".into(),
        ))
    }
    /// ADR 0014 issue #1/#2: commit a snapshot whose post-snapshot
    /// pipeline has fully succeeded. See `SandboxBackend::commit_snapshot`
    /// for the contract. Default impl returns Ok so HostClients backed by
    /// backends that don't need a commit phase (Process, VZ-dev) work
    /// unchanged.
    async fn commit_snapshot(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    /// ADR 0014 issue #1/#2: abort a snapshot whose downstream pipeline
    /// failed. Idempotent. See `SandboxBackend::abort_snapshot`.
    async fn abort_snapshot(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError>;

    /// ADR 0020 P1: boot the image to agentd-ready, snapshot it, tear
    /// the capture VM down, and return the portable snapshot metadata.
    /// Called by the coord during `POST /api/enabled-images` to produce
    /// the per-image base snapshot `create_session` restores from.
    /// Default errors so mocks / non-FC hosts opt out; the local +
    /// gRPC clients delegate to the backend's `build_base_snapshot`.
    ///
    /// `warm` is the image's optional capture-time prewarm hook
    /// ([`WarmConfig`]), threaded down to the backend. `capture_env` is the
    /// resolved capture-time env injected into the warm hook (refs already
    /// resolved coordinator-side).
    async fn build_base_snapshot(
        &self,
        _spec: SandboxSpec,
        _warm: Option<WarmConfig>,
        _capture_env: std::collections::HashMap<String, String>,
    ) -> Result<SnapshotMetadata, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this host doesn't support `build_base_snapshot`".into(),
        ))
    }

    /// ADR 0020 P1: restore a per-image base snapshot for a session
    /// and inject the per-session env. Called by `create_session` on a
    /// base-snapshot hit. ADR 0021 P1.5 retired the option-D
    /// substrate-swap stage that used to follow the restore — the
    /// harness lives in the rootfs now. Default errors so mocks /
    /// non-FC hosts opt out.
    async fn restore_base_for_session(
        &self,
        _metadata: SnapshotMetadata,
        _session_env: std::collections::HashMap<String, String>,
        _selected_mounts: Vec<crate::types::sandbox::AuxRoDrive>,
    ) -> Result<SandboxId, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this host doesn't support `restore_base_for_session`".into(),
        ))
    }

    /// ADR 0013: bundle the egress policy with the agent spawn so the
    /// host applies the policy to its egress proxy registry BEFORE
    /// starting the agent process. Atomic by construction —
    /// eliminates the WS-era frame-ordering invariant that the
    /// stateless transport can't honour. Local impl applies in-proc;
    /// the gRPC impl ships both fields in a single `StartAgent` RPC.
    async fn start_agent(
        &self,
        id: SandboxId,
        agent: AgentSpec,
        policy: SessionEgressPolicy,
    ) -> Result<(), SandboxError>;

    /// Apply an egress policy to the host's local proxy registry
    /// without spawning an agent. The companion to `start_agent`'s
    /// bundled form, for sessions that don't carry a harness but
    /// can still emit outbound traffic via raw `/exec`. Idempotent:
    /// the host's egress registry keys on
    /// `(session_id, sandbox_id, guest_ip)` and upserts.
    async fn apply_egress_policy(&self, policy: SessionEgressPolicy) -> Result<(), SandboxError>;

    async fn guest_ip(&self, id: SandboxId) -> Option<String>;

    // ---- harness routing ----
    /// Tell this host that an upcoming harness connection identifying
    /// itself with `session_id` should be routed to `sandbox_id`.
    /// Errors are infallible locally (the local hub just inserts into
    /// a HashMap); remote impls swallow transport failures into a
    /// warning log because the harness can still attach via the
    /// session-id lookup path on its end.
    async fn bind_session(&self, session_id: SessionId, sandbox_id: SandboxId);

    /// Drop the session→sandbox binding.
    async fn unbind_session(&self, session_id: SessionId);

    /// Forward a user prompt to the attached harness for `sandbox_id`.
    /// `prompt_id` is the client/coord-minted id that correlates this
    /// prompt with its eventual `RunStarted{prompt_id}` (and, if queued
    /// behind an in-flight run, the `PromptQueued`/`PromptEdited`/
    /// `PromptDequeued` events). `SandboxError::NotFound` if no harness is
    /// bound (call `ensure_active` upstream to auto-resume). Other errors
    /// come from the underlying writer dropping or the harness
    /// disconnecting mid-send.
    async fn send_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
    ) -> Result<(), SandboxError>;

    /// Phase 1b: edit a still-queued type-ahead prompt on the attached
    /// harness by its `prompt_id`, before the harness consumes it. No-op
    /// once consumed (the harness is the single writer). Default is a
    /// no-op for impls without a real harness (test fakes); the
    /// `HostRegistry`, gRPC client, and `LocalHostClient` override it.
    async fn edit_queued_prompt(
        &self,
        _sandbox_id: SandboxId,
        _prompt_id: String,
        _text: String,
    ) -> Result<(), SandboxError> {
        Ok(())
    }

    /// Phase 1b: remove a still-queued type-ahead prompt by its
    /// `prompt_id` (the user pulled it back to the composer or cancelled),
    /// before consumption. No-op once consumed. Default no-op for test
    /// fakes; the real impls override it.
    async fn dequeue_queued_prompt(
        &self,
        _sandbox_id: SandboxId,
        _prompt_id: String,
    ) -> Result<(), SandboxError> {
        Ok(())
    }

    /// ADR 0054: deliver a user's answer to a deferred AskUserQuestion to
    /// the attached harness for `sandbox_id` (which stashes it and re-fires
    /// the deferred tool via `--resume`). `answers` is keyed by question
    /// text, each value the selected labels (1 for single-select, N for
    /// multi-select) — the `BTreeMap` form of `engram_harness_proto::Answers`
    /// (spelled out here to keep `engram-core` free of a harness-proto
    /// dependency cycle). Default is a no-op for harness-less fakes; the
    /// `HostRegistry`, gRPC client, and `LocalHostClient` override it.
    async fn answer_question(
        &self,
        _sandbox_id: SandboxId,
        _tool_call_id: String,
        _answers: std::collections::BTreeMap<String, Vec<String>>,
    ) -> Result<(), SandboxError> {
        Ok(())
    }

    /// ADR 0030: operator interrupt — stop the in-flight run on the
    /// attached harness for `sandbox_id` while keeping the session
    /// alive. The harness SIGINTs its current child and returns to Idle;
    /// the next prompt resumes the same conversation via `--resume`.
    /// `SandboxError::NotFound` if no harness is bound. Default is a
    /// no-op for impls without a real harness (test fakes); the
    /// `HostRegistry`, gRPC client, and `LocalHostClient` override it.
    async fn interrupt(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }

    /// Track A: non-destructive harness re-handshake — tell the attached
    /// harness for `sandbox_id` to drop + re-dial its host connection so
    /// the re-attach re-emits `Idle`, resyncing a session whose event
    /// stream desynced from the run state machine. The running agent is
    /// untouched. Used by the coordinator's desync watchdog. `NotFound`
    /// if no harness is bound. Default no-op for harness-less fakes; the
    /// `HostRegistry`, gRPC client, and `LocalHostClient` override it.
    async fn rehandshake(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }

    /// ADR 0045 Phase F: freeze the running microVM for `sandbox_id`
    /// *in place* — pause its vCPUs without snapshotting, destroying, or
    /// changing session state. An admin affordance to drive + observe
    /// the pause/flush path (and the test surface for the live-migration
    /// work). Default no-op for harness-less fakes; the `HostRegistry`,
    /// gRPC client, and `LocalHostClient` override it to reach the
    /// backend's [`crate::traits::SandboxBackend::pause`].
    async fn pause(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }

    /// ADR 0045 Phase F: unfreeze a [`Self::pause`]d microVM — resume
    /// its vCPUs in place. Symmetric with `pause`; same overrides.
    async fn resume(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }

    /// ADR 0013 + ADR 0011 follow-up #3: pin a sandbox against idle
    /// eviction while a shell WebSocket is open. The local hub is the
    /// only source of truth for "is a shell attached to this sandbox?"
    /// — the in-proc `LocalHostClient` reaches its hub directly;
    /// remote impls route to the host that owns the harness session.
    /// Reference-counted in the hub so a future second client doesn't
    /// decrement to zero prematurely.
    async fn acquire_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError>;

    /// Release a `acquire_shell` reference. Called on shell bridge
    /// exit (success or error). Symmetric with `acquire_shell`; the
    /// hub silently swallows underflow rather than erroring so a buggy
    /// caller can't poison the count.
    async fn release_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError>;

    /// Issue #219: refresh the keep-alive stamp on an existing shell
    /// pin. The coord shell bridge calls this periodically (piggybacking
    /// its WS keepalive) so the host can distinguish a live shell from
    /// one whose coord-side bridge task died without sending
    /// `release_shell` (rolling deploy, crash, dropped WS). The host's
    /// eviction tick reaps pins not renewed within its stale window,
    /// closing the "pinned forever" leak. A no-op if no pin exists for
    /// the sandbox — renewal must never resurrect a released pin.
    ///
    /// Default impl is a no-op so backends that don't own a hub (mocks,
    /// remote impls in tests) need no change; the in-proc
    /// `LocalHostClient` and the gRPC client override it.
    async fn renew_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        let _ = sandbox_id;
        Ok(())
    }

    /// ADR 0014 issue #6: open a bidi shell tunnel to the in-guest
    /// `ttyd` for `sandbox_id`. The returned [`ShellTunnel`] is a
    /// pair of mpsc channels:
    /// - `outbound`: caller (coord) sends WS frames here; the
    ///   implementation forwards each frame to ttyd inside the guest.
    /// - `inbound`: implementation pushes WS frames received from
    ///   ttyd here; caller drains and forwards to the browser.
    ///
    /// On either channel closing (browser disconnect, ttyd exit,
    /// transport error) the implementation drops both halves and
    /// frees its proxy task; idempotent. The first-frame `sandbox_id`
    /// convention on the wire (gRPC ProxyShellFrame) lives in the
    /// gRPC client/server only — this trait surface takes the
    /// `sandbox_id` directly so trait callers don't have to embed it
    /// in the frame stream.
    ///
    /// Default impl errors with `NotFound`: only host-agent
    /// implementations actually proxy shells. The Local impl in the
    /// host-agent opens a WebSocket to `ws://<guest_ip>:7681/ws`
    /// inside the right network namespace (per
    /// [`SandboxBackend::netns_name_for`]) and bridges; the gRPC
    /// client impl opens a gRPC bidi stream and bridges the channels
    /// with the wire frames.
    async fn proxy_shell(&self, sandbox_id: SandboxId) -> Result<ShellTunnel, SandboxError> {
        let _ = sandbox_id;
        Err(SandboxError::NotFound)
    }

    /// ADR 0064: open a bidi RAW-BYTE tunnel to an arbitrary guest TCP
    /// `port` for `sandbox_id` (a dev server the agent started). The
    /// raw-byte sibling of [`Self::proxy_shell`]: the returned
    /// [`PortTunnel`] is a pair of mpsc channels — `outbound` (caller →
    /// guest socket) and `inbound` (guest socket → caller) — and either
    /// channel closing tears the tunnel down.
    ///
    /// Unlike `proxy_shell` there is no host-side `start_shell` step: the
    /// service on `port` is user/agent-managed, not host-spawned, so the
    /// host just dials `guest_ip:port` in the right netns (a short
    /// connection-refused retry covers the just-started race).
    ///
    /// Default errors with `NotFound`: only host-agent implementations
    /// proxy ports. The Local impl dials inside the per-VM netns; the
    /// gRPC client impl opens a `ProxyPort` bidi stream and bridges the
    /// channels with the wire `data`/`close` frames.
    async fn proxy_port(
        &self,
        sandbox_id: SandboxId,
        port: u16,
    ) -> Result<PortTunnel, SandboxError> {
        let _ = (sandbox_id, port);
        Err(SandboxError::NotFound)
    }

    /// How a harness process inside this host's sandboxes dials back
    /// to the harness channel. Static per-host capability — drives
    /// the argv shape `resolve_harness` builds. Default `Vsock`
    /// because FC + VZ are the production backends.
    fn harness_dial(&self) -> HarnessDial {
        HarnessDial::Vsock
    }

    /// Register a harness sink on this host's local VMM backend. Only
    /// meaningful for in-process impls — `LocalHostClient` forwards
    /// the closure straight to its inner `SandboxBackend`. Remote impls
    /// no-op: the host on the other end of the wire wires its own
    /// local sink internally, and the closure (capturing coord-side
    /// state) wouldn't serialize anyway.
    fn set_harness_sink(&self, _sink: HarnessSink) {}

    /// ADR 0023: register a forge sink on this host's local backend.
    /// Same in-process-only semantics as [`set_harness_sink`](Self::set_harness_sink)
    /// — `LocalHostClient` forwards to its inner `SandboxBackend`;
    /// remote impls no-op.
    fn set_forge_sink(&self, _sink: ForgeSink) {}

    /// ADR 0026: register an artifact-upload sink on this host's local
    /// backend. Same in-process-only semantics as
    /// [`set_forge_sink`](Self::set_forge_sink) — `LocalHostClient`
    /// forwards to its inner `SandboxBackend`; remote impls no-op.
    fn set_upload_sink(&self, _sink: UploadSink) {}

    /// ADR 0016 Phase A: per-sandbox COW diagnostic snapshot.
    /// `None` if this host doesn't have a chunk-tracked view of
    /// the sandbox (backend not NBD-attached, or the
    /// `sandbox_id` isn't bound here). The caller — `cow_state`
    /// fan-out at coord — treats `Ok(None)` and `Err(NotFound)`
    /// uniformly. Default returns `None` so `HostClient` impls
    /// that don't forward this can compile without changes.
    async fn cow_state(&self, _id: SandboxId) -> Result<Option<CowState>, SandboxError> {
        Ok(None)
    }

    /// ADR 0016 Phase A: bulk variant. One record per chunk-tracked
    /// sandbox this host owns. Cheaper than `list() + N ×
    /// cow_state()` over the wire — single RPC, single map walk on
    /// the host. Default empty so non-host-agent impls (mocks,
    /// in-proc test glue) compile.
    async fn cow_state_all(&self) -> Result<Vec<CowStateRecord>, SandboxError> {
        Ok(Vec::new())
    }

    /// ADR 0016 Phase B commit 4a — admin trigger for the
    /// FlushScheduler primitive. Coord's
    /// `POST /api/admin/sessions/:id/flush-now` calls this on the
    /// session's bound host to force a flush + manifest publish.
    /// Returns the new `ManifestRef` if any chunks were drained;
    /// `None` if the sandbox isn't chunk-tracked or no dirty bytes
    /// were buffered.
    ///
    /// Default `Ok(None)` so mocks and the warm-start scheduler-less
    /// pre-Phase-B impls compile unchanged.
    async fn flush_sandbox(
        &self,
        _id: SandboxId,
    ) -> Result<Option<crate::types::manifest::ManifestRef>, SandboxError> {
        Ok(None)
    }
}
