//! Identity-only durable creation and explicit cold-recovery producer.
use crate::{error::ApiError, state::SharedState};
use engram_core::types::managed_bridge::conflict;
use engram_core::{
    types::{managed_boot::*, managed_bridge::*, managed_cold::*, managed_deny::*, sandbox::*},
    SessionId, SnapshotId,
};

pub(crate) async fn prepare(
    state: &SharedState,
    q: ManagedCreationQuery,
) -> Result<ManagedCreationState, ApiError> {
    q.validate()?;
    let meta = &state.services.meta;
    let view = meta.read_managed_creation_state(&q).await?;
    if matches!(
        view.state,
        ManagedCreationPhase::Absent | ManagedCreationPhase::Terminal
    ) {
        return Err(conflict().into());
    }
    if view.boot.is_none()
        && meta
            .read_managed_boot_intent(&q, "initial")
            .await?
            .is_none()
    {
        let selection = meta.read_managed_boot_selection(&q).await?;
        let intent = ManagedBootIntent {
            query: q.clone(),
            intent_id: state.services.entropy.uuid().to_string(),
            revision: 1,
            selection,
            sandbox_id: state.services.entropy.uuid().into(),
            binding_epoch: 1,
            disk_manifest: engram_core::types::ManifestRef {
                manifest_id: state.services.entropy.uuid(),
                version: 1,
            },
            recovery: None,
            host_id: None,
            assets: None,
            completed: false,
        };
        meta.begin_managed_boot_intent(intent).await?;
    }
    meta.read_managed_creation_state(&q)
        .await
        .map_err(Into::into)
}
pub(crate) async fn recover(
    state: &SharedState,
    input: ManagedRecoveryInput,
) -> Result<ManagedCreationState, ApiError> {
    input.validate()?;
    let meta = &state.services.meta;
    let q = &input.query;
    let operation = format!("recover:{}", input.recovery_id);
    if let Some(i) = meta.read_managed_boot_intent(q, &operation).await? {
        if i.recovery
            .as_ref()
            .is_none_or(|r| r.source != input.expected_source_key)
        {
            return Err(conflict().into());
        }
        let mut view = meta.read_managed_creation_state(q).await?;
        view.operation = Some(i.view());
        return Ok(view);
    }
    let row = meta.read_managed_boot(&input.expected_source_key).await?;
    if row.prepared.is_none() {
        return Err(conflict().into());
    }
    let mut selection = meta.read_managed_boot_selection(q).await?;
    selection.image = row.request.cold.image.clone();
    selection.aux_ro_drives = row.request.cold.aux_ro_drives.clone();
    let spec = row.request.spec()?;
    selection.vcpus = spec.cpu.vcpus;
    selection.memory_mib = spec.memory.max_mib;
    selection.disk_gib = spec.disk.max_gib;
    let capture_id = SnapshotId::from(state.services.entropy.uuid());
    let intent = ManagedBootIntent {
        query: q.clone(),
        intent_id: state.services.entropy.uuid().to_string(),
        revision: 1,
        selection,
        sandbox_id: state.services.entropy.uuid().into(),
        binding_epoch: input
            .expected_source_key
            .context
            .target
            .binding_epoch
            .checked_add(1)
            .ok_or_else(conflict)?,
        disk_manifest: engram_core::types::ManifestRef {
            manifest_id: capture_id.as_uuid(),
            version: 1,
        },
        recovery: Some(ManagedRecoveryIntent {
            recovery_id: input.recovery_id,
            source: input.expected_source_key,
            capture_id,
        }),
        host_id: None,
        assets: None,
        completed: false,
    };
    meta.begin_managed_boot_intent(intent).await?;
    meta.read_managed_creation_state(q)
        .await
        .map_err(Into::into)
}
async fn publication(
    state: &SharedState,
) -> Result<engram_core::types::managed_operator::OperatorGeneration, ApiError> {
    let p = state.services.meta.read_operator_publication().await?;
    if !p.admission_open || p.pending.is_some() {
        return Err(conflict().into());
    }
    p.published.ok_or_else(|| conflict().into())
}
/// One durable phase per pass. All host/blob work occurs after short store transactions.
pub(crate) async fn drive(state: &SharedState, i: ManagedBootIntent) -> Result<(), ApiError> {
    if i.completed {
        return Ok(());
    }
    i.validate()?;
    let meta = &state.services.meta;
    let generation = publication(state).await?;
    // Lost claim replies are reconciled before any capture or private disk operation.
    if let Some(b) = meta.get_managed_boot(&i.query.identity).await? {
        if b.request.key.boot_attempt == i.intent_id {
            if b.request.key.context.target.sandbox_id != i.sandbox_id {
                return Err(conflict().into());
            }
            meta.advance_managed_boot_intent(
                &i.intent_id,
                i.revision,
                ManagedIntentAdvance::Complete,
            )
            .await?;
            return Ok(());
        }
    }
    let capture = if let Some(r) = &i.recovery {
        Some(
            crate::managed_boot::capture(
                state,
                ManagedCaptureRequest {
                    key: r.source.clone(),
                    capture_id: r.capture_id,
                },
            )
            .await?,
        )
    } else {
        None
    };
    if i.host_id.is_none() {
        let (repo, tag) = engram_core::types::session::split_image_ref(&i.selection.image);
        let bundles: Vec<_> = i
            .selection
            .aux_ro_drives
            .iter()
            .map(|d| AuxBundleRef {
                drive_id: d.drive_id.clone(),
                sha256: d.sha256.clone().expect("validated selection"),
            })
            .collect();
        let context = crate::placement::ScheduleContext {
            nbd_slot_need: 1,
            repo,
            image_version: tag,
            snapshot_host: None,
            memory_mib: Some(i.selection.memory_mib),
            cpu_budget_vcpus: Some(i.selection.vcpus),
            required_image_digest: Some(engram_protocol::heartbeat::ManifestDigest::new(
                i.selection.image_digest.clone(),
            )),
            exclude_host: None,
            prefer_host: None,
            caps: Default::default(),
            prefer_bundles: &bundles,
        };
        let candidates = crate::placement::candidates_for(
            meta.as_ref(),
            &context,
            state.services.clock.now_utc(),
        )
        .await
        .map_err(engram_core::SandboxError::from)?;
        for host in candidates.hosts {
            let h = meta.get_host(host).await?.ok_or_else(conflict)?;
            if !matches!(h.capabilities.backend.as_str(), "firecracker" | "vz") {
                continue;
            }
            meta.advance_managed_boot_intent(
                &i.intent_id,
                i.revision,
                ManagedIntentAdvance::SelectHost(host),
            )
            .await?;
            return Ok(());
        }
        return Ok(()); // Durable pending capacity; scanner retries the same intent.
    }
    if capture.is_none() {
        fork(state, &i).await?;
    }
    if i.assets.is_none() {
        let request = i.assets_request()?;
        let assets = state
            .services
            .host
            .read_managed_cold_assets(request.clone())
            .await?;
        assets.validate_for(&request)?;
        let h = meta.get_host(request.host_id).await?.ok_or_else(conflict)?;
        let backend = match assets.backend {
            engram_core::types::managed_assets::ManagedColdBackend::Firecracker => "firecracker",
            engram_core::types::managed_assets::ManagedColdBackend::Vz => "vz",
        };
        if h.capabilities.backend != backend {
            return Err(conflict().into());
        }
        if let Some(a) = &capture {
            if assets.disk_sha256 != a.disk_sha256
                || assets.disk_size_bytes != a.disk_size_bytes
                || assets.kernel_sha256 != a.kernel_sha256
            {
                return Err(conflict().into());
            }
        }
        meta.advance_managed_boot_intent(
            &i.intent_id,
            i.revision,
            ManagedIntentAdvance::Assets(Box::new(assets)),
        )
        .await?;
        return Ok(());
    }
    let assets = i.assets.as_ref().ok_or_else(conflict)?;
    let prior =
        crate::api::sessions::load_session_policy(state, i.query.identity.session_id).await?;
    let old = prior
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| conflict())?;
    let (_, deny) = engram_core::types::managed_operator::retain(
        old.as_deref(),
        generation.candidate.hosts()?,
    )?;
    let key = ManagedBootKey {
        creation: i.query.identity.clone(),
        native_digest: i.query.native_digest.clone(),
        boot_attempt: i.intent_id.clone(),
        context: ManagedPolicyContext {
            target: ManagedPolicyTarget {
                session_id: i.query.identity.session_id,
                host_id: i.host_id.ok_or_else(conflict)?,
                sandbox_id: i.sandbox_id,
                binding_epoch: i.binding_epoch,
            },
            operator_generation: generation.generation,
            operator_identity: generation.identity,
            binding_hash: None,
        },
    };
    let source = match &capture {
        None => ManagedColdSource::FreshImage {
            base_manifest: i.selection.base_manifest,
        },
        Some(a) => ManagedColdSource::Capture {
            capture_id: a.request.capture_id,
            session_id: i.query.identity.session_id,
            sandbox_id: a.request.key.context.target.sandbox_id,
            binding_epoch: a.request.key.context.target.binding_epoch,
            record_digest: a.record_digest.clone(),
        },
    };
    let cold = ManagedColdPrepare {
        version: 1,
        session_id: i.query.identity.session_id,
        sandbox_id: i.sandbox_id,
        boot_attempt: i.intent_id.clone(),
        image: i.selection.image.clone(),
        disk: ManagedColdDisk {
            manifest: i.disk_manifest,
            sha256: assets.disk_sha256.clone(),
            size_bytes: assets.disk_size_bytes,
            source,
        },
        kernel_sha256: assets.kernel_sha256.clone(),
        aux_ro_drives: i.selection.aux_ro_drives.clone(),
    };
    // A logical manifest locator, replaced by authenticated host materialization; never a local path supplied by O.
    let spec = SandboxSpec {
        image: i.selection.image.clone(),
        rootfs_source: Some(i.disk_manifest.storage_key().into()),
        rootfs_manifest: Some(i.disk_manifest),
        image_uri: None,
        cpu: CpuLimit {
            vcpus: i.selection.vcpus,
        },
        memory: MemoryLimit {
            max_mib: i.selection.memory_mib,
        },
        disk: DiskLimit {
            max_gib: i.selection.disk_gib,
        },
        ttl: None,
        env: Default::default(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: i.selection.aux_ro_drives.clone(),
        swap_mib: None,
        swap_source: None,
        swap_manifest: None,
    };
    let request = ManagedBootPrepare {
        key,
        cold,
        bootstrap_policy: deny,
        capture_authority: capture,
        sandbox_spec_json: serde_json::to_string(&spec).map_err(|_| conflict())?,
    };
    crate::managed_boot::register(state, request).await?;
    meta.advance_managed_boot_intent(&i.intent_id, i.revision, ManagedIntentAdvance::Complete)
        .await?;
    Ok(())
}
async fn fork(state: &SharedState, i: &ManagedBootIntent) -> Result<(), ApiError> {
    let store = &state.services.chunk_store;
    let mut expected = store
        .get_manifest(i.selection.base_manifest)
        .await
        .map_err(|_| unavailable())?;
    if expected.kind != engram_chunk_store::ManifestKind::Disk {
        return Err(conflict().into());
    }
    expected.parent = Some(i.selection.base_manifest);
    expected.working_set_trace = None;
    // PUT is create-only. A lost reply is reconciled by complete immutable content.
    let written = store.put_manifest(i.disk_manifest, &expected).await;
    if written.is_err() {
        let actual = store
            .get_manifest(i.disk_manifest)
            .await
            .map_err(|_| unavailable())?;
        if serde_json::to_value(actual).map_err(|_| conflict())?
            != serde_json::to_value(expected).map_err(|_| conflict())?
        {
            return Err(conflict().into());
        }
    }
    Ok(())
}
fn unavailable() -> ApiError {
    ApiError::Unavailable("managed disk storage unavailable".into())
}
pub(crate) async fn run_once(state: &SharedState) -> Result<(), ApiError> {
    let mut after: Option<SessionId> = None;
    loop {
        let rows = state
            .services
            .meta
            .list_managed_boot_intents(after, 128)
            .await?;
        if rows.is_empty() {
            break;
        }
        for i in rows {
            after = Some(i.query.identity.session_id);
            if let Err(error) = drive(state, i).await {
                tracing::debug!(error=%error,"managed preparation remains closed");
            }
        }
    }
    Ok(())
}
