//! Durable managed boot driver. Every transaction ends before a host command.
use crate::{error::ApiError, state::SharedState};
use engram_core::{
    types::{managed_boot::*, managed_create::digest, mcp_policy::WriteEligibility},
    SandboxError, SessionId,
};
use engram_protocol::grpc_client::restore_rpc_timeout;
use std::{future::Future, time::Duration};

// Expiry leaves the host outcome unknown. Detached commands can still complete;
// the durable phase decides whether to retry the same command or only read it.
pub(crate) async fn host_rpc<T>(
    budget: Duration,
    call: impl Future<Output = Result<T, SandboxError>>,
) -> Result<T, SandboxError> {
    tokio::time::timeout(budget, call).await.map_err(|_| {
        SandboxError::Unavailable("managed host request deadline exceeded; outcome unknown".into())
    })?
}

pub(crate) async fn gate(
    state: &SharedState,
    id: SessionId,
    allow_started: bool,
) -> Result<(), ApiError> {
    match state.services.meta.managed_boot_open(id).await? {
        None => Ok(()),
        Some(true) if allow_started => Ok(()),
        _ => Err(ApiError::Unavailable(
            "managed session requires its registered boot authority".into(),
        )),
    }
}
/// Called by the trusted durable preparation driver after physical input verification.
pub async fn register(
    state: &SharedState,
    request: ManagedBootPrepare,
) -> Result<ManagedBootRecord, ApiError> {
    Ok(state.services.meta.claim_managed_boot(request).await?)
}

pub(crate) async fn finalize(
    state: &SharedState,
    input: ManagedBootFinalizeInput,
) -> Result<ManagedBootRecord, ApiError> {
    let meta = &state.services.meta;
    let sid = input.key.creation.session_id;
    let old = meta.read_managed_boot(&input.key).await?;
    if let Some(f) = &old.finalization {
        if f.prepared.receipt_id == input.prepared_receipt_id
            && f.policy == input.policy
            && f.native_version == input.native_version
            && f.combined_manifest == input.combined_manifest
            && f.combined_manifest_digest == input.combined_manifest_digest
        {
            return Ok(old);
        }
        return Err(conflict().into());
    }
    let prepared = old.prepared.clone().ok_or_else(conflict)?;
    if old.phase != ManagedBootPhase::Prepared || prepared.receipt_id != input.prepared_receipt_id {
        return Err(conflict().into());
    }
    let applied = host_rpc(
        restore_rpc_timeout(),
        state
            .services
            .host
            .read_managed_policy(input.key.context.target.clone()),
    )
    .await?;
    if applied.as_ref() != Some(&input.policy) {
        return Err(conflict().into());
    }
    let native = meta
        .read_managed_creation(&input.key.creation)
        .await?
        .native;
    validate_combined_manifest(&native, &input.combined_manifest)?;
    let policy = meta.read_mcp_write_policy(sid).await?;
    if !policy.current {
        return Err(conflict().into());
    }
    let selection = meta
        .read_managed_boot_selection(&engram_core::types::managed_bridge::ManagedCreationQuery {
            identity: input.key.creation.clone(),
            native_version: native.version,
            native_digest: native.digest.clone(),
        })
        .await?;
    if selection.aux_ro_drives != old.request.cold.aux_ro_drives
        || meta.get_session_harness(sid).await?.as_deref()
            != Some(selection.launch.descriptor.name.as_str())
    {
        return Err(conflict().into());
    }
    let mut session_env = crate::api::sessions::load_session_secrets(state, sid)
        .await?
        .ok_or_else(conflict)?;
    session_env.insert("ENGRAM_SESSION_ID".into(), sid.to_string());
    let mode = (policy.write_eligibility != WriteEligibility::WriteEligible).then_some("plan");
    let resolved = crate::api::sessions::resolve_pinned_harness(
        state,
        selection.launch,
        selection
            .aux_ro_drives
            .first()
            .ok_or_else(conflict)?
            .clone(),
        sid,
        session_env,
        mode,
    )?;
    let mut agent = resolved.agent;
    crate::api::sessions::inject_harness_env(state, sid, &mut agent.env).await?;
    bind_agent_target(&mut agent, &input.key)?;
    agent.session_env.remove("ENGRAM_TOOLS");
    agent.env.insert(
        "ENGRAM_TOOLS".into(),
        String::from_utf8(input.combined_manifest.clone()).map_err(|_| conflict())?,
    );
    let agent_bytes = serde_json::to_vec(&serde_json::to_value(&agent).map_err(|_| conflict())?)
        .map_err(|_| conflict())?;
    let finalization = ManagedBootFinalization {
        prepared,
        policy: input.policy,
        native_version: input.native_version,
        combined_manifest: input.combined_manifest,
        combined_manifest_digest: input.combined_manifest_digest,
        agent_digest: digest(&agent_bytes),
    };
    finalization.validate(&native)?;
    ManagedBootFinalize {
        finalization: finalization.clone(),
        agent,
    }
    .validate_agent()?;
    let sealed = engram_crypto::CredCipher::new(state.services.kek.as_ref())
        .seal(&agent_bytes)
        .await
        .map_err(|_| ApiError::Unavailable("managed environment sealing unavailable".into()))?;
    let sealed_agent = ManagedBootSealedAgent {
        wrapped_dek: sealed.wrapped_dek,
        nonce: sealed.nonce.to_vec(),
        ciphertext: sealed.ciphertext,
        key_id: sealed.key_id,
    };
    Ok(meta
        .advance_managed_boot(
            &input.key,
            old.revision,
            ManagedBootAdvance::Finalize {
                finalization: Box::new(finalization),
                sealed_agent,
                write_policy: policy,
            },
        )
        .await?)
}

/// Bind attach identity before canonical hashing or sealing. Session-wide values
/// must not override the harness-only token, even when the strings happen to match.
fn bind_agent_target(
    agent: &mut engram_core::types::sandbox::AgentSpec,
    key: &ManagedBootKey,
) -> Result<(), engram_core::MetaError> {
    key.validate()?;
    let token = engram_harness_proto::attach_token::AttachToken {
        sandbox_id: key.context.target.sandbox_id,
        binding_epoch: key.context.target.binding_epoch,
    };
    let env = token.env();
    if env
        .iter()
        .any(|(name, _)| agent.session_env.contains_key(name))
    {
        return Err(conflict());
    }
    agent.binding_epoch = token.binding_epoch;
    agent.env.extend(env);
    Ok(())
}

async fn open_finalize(
    state: &SharedState,
    row: &ManagedBootRecord,
) -> Result<ManagedBootFinalize, ApiError> {
    let sealed = row.sealed_agent.as_ref().ok_or_else(conflict)?;
    let sealed = engram_crypto::SealedCred {
        wrapped_dek: sealed.wrapped_dek.clone(),
        nonce: sealed.nonce.as_slice().try_into().map_err(|_| conflict())?,
        ciphertext: sealed.ciphertext.clone(),
        key_id: sealed.key_id.clone(),
    };
    let bytes = engram_crypto::CredCipher::new(state.services.kek.as_ref())
        .open(&sealed)
        .await
        .map_err(|_| ApiError::Unavailable("managed environment unavailable".into()))?;
    let command = ManagedBootFinalize {
        finalization: row.finalization.clone().ok_or_else(conflict)?,
        agent: serde_json::from_slice(&bytes).map_err(|_| conflict())?,
    };
    command.validate_agent()?;
    Ok(command)
}

/// Each call performs at most one phase. A failed host response leaves authority closed.
pub async fn drive(state: &SharedState, key: &ManagedBootKey) -> Result<(), ApiError> {
    drive_with_timeout(state, key, restore_rpc_timeout()).await
}

async fn drive_with_timeout(
    state: &SharedState,
    key: &ManagedBootKey,
    rpc_timeout: Duration,
) -> Result<(), ApiError> {
    let row = state.services.meta.read_managed_boot(key).await?;
    if matches!(
        row.phase,
        ManagedBootPhase::Prepared | ManagedBootPhase::Finalizing | ManagedBootPhase::Finalized
    ) {
        // Final policy installation can retire a prepared owner. Preserve its
        // exact nonrunning proof even when no finalization/start reply succeeds.
        if let ManagedBootHostState::Retired { receipt } = host_rpc(
            rpc_timeout,
            state.services.host.read_managed_boot(key.clone()),
        )
        .await?
        {
            state
                .services
                .meta
                .advance_managed_boot(key, row.revision, ManagedBootAdvance::Retired(receipt))
                .await?;
            return Ok(());
        }
    }
    match row.phase {
        ManagedBootPhase::Claimed => {
            let receipt = host_rpc(
                rpc_timeout,
                state
                    .services
                    .host
                    .prepare_managed_boot(row.request.clone()),
            )
            .await?;
            state
                .services
                .meta
                .advance_managed_boot(
                    key,
                    row.revision,
                    ManagedBootAdvance::Prepared(Box::new(receipt)),
                )
                .await?;
        }
        ManagedBootPhase::Finalizing => {
            let command = open_finalize(state, &row).await?;
            let receipt = host_rpc(
                rpc_timeout,
                state.services.host.finalize_managed_boot(command),
            )
            .await?;
            state
                .services
                .meta
                .advance_managed_boot(key, row.revision, ManagedBootAdvance::Finalized(receipt))
                .await?;
        }
        ManagedBootPhase::Finalized => {
            let receipt = row.receipt.clone().ok_or_else(conflict)?;
            let start = ManagedBootStart {
                start_id: digest(format!("{}:{}", key.boot_attempt, receipt.receipt_id).as_bytes()),
                receipt,
            };
            let issued = state
                .services
                .meta
                .advance_managed_boot(
                    key,
                    row.revision,
                    ManagedBootAdvance::IssueStart(start.clone()),
                )
                .await?;
            // Only this successful CAS holder can issue start. Failure or lost reply
            // is resolved by the read branch, never by another start command.
            let observed =
                host_rpc(rpc_timeout, state.services.host.start_managed_boot(start)).await?;
            confirm_started(state, &issued, observed).await?;
        }
        ManagedBootPhase::StartIssued => {
            let observed = host_rpc(
                rpc_timeout,
                state.services.host.read_managed_boot(key.clone()),
            )
            .await?;
            confirm_started(state, &row, observed).await?;
        }
        ManagedBootPhase::Started => {
            let observed = match host_rpc(
                rpc_timeout,
                state.services.host.read_managed_boot(key.clone()),
            )
            .await
            {
                Ok(observed) => observed,
                Err(error) => {
                    state
                        .services
                        .meta
                        .advance_managed_boot(key, row.revision, ManagedBootAdvance::Interrupt)
                        .await?;
                    return Err(error.into());
                }
            };
            match observed {
                ManagedBootHostState::Started { intent } if Some(&intent) == row.start.as_ref() => {
                }
                ManagedBootHostState::Retired { receipt } => {
                    state
                        .services
                        .meta
                        .advance_managed_boot(
                            key,
                            row.revision,
                            ManagedBootAdvance::Retired(receipt),
                        )
                        .await?;
                }
                _ => {
                    state
                        .services
                        .meta
                        .advance_managed_boot(key, row.revision, ManagedBootAdvance::Interrupt)
                        .await?;
                }
            }
        }
        ManagedBootPhase::Prepared | ManagedBootPhase::Interrupted => {}
    }
    Ok(())
}
async fn confirm_started(
    state: &SharedState,
    row: &ManagedBootRecord,
    observed: ManagedBootHostState,
) -> Result<(), ApiError> {
    match observed {
        ManagedBootHostState::Retired { receipt } => {
            state
                .services
                .meta
                .advance_managed_boot(
                    &row.request.key,
                    row.revision,
                    ManagedBootAdvance::Retired(receipt),
                )
                .await?;
            Ok(())
        }
        ManagedBootHostState::Started { intent } if Some(&intent) == row.start.as_ref() => {
            state
                .services
                .meta
                .advance_managed_boot(
                    &row.request.key,
                    row.revision,
                    ManagedBootAdvance::Started(intent),
                )
                .await?;
            state.outbox_wake.notify_one();
            Ok(())
        }
        ManagedBootHostState::StartPending { intent } if Some(&intent) == row.start.as_ref() => {
            Ok(())
        }
        _ => Err(conflict().into()),
    }
}
pub async fn run_once(state: &SharedState) -> Result<(), ApiError> {
    run_once_with_timeout(state, restore_rpc_timeout()).await
}

async fn run_once_with_timeout(state: &SharedState, rpc_timeout: Duration) -> Result<(), ApiError> {
    crate::managed_bridge::run_once(state).await?;
    let mut after = None;
    loop {
        let keys = state.services.meta.list_managed_boots(after, 128).await?;
        if keys.is_empty() {
            break;
        }
        for key in keys {
            after = Some(key.creation.session_id);
            if let Err(error) = drive_with_timeout(state, &key, rpc_timeout).await {
                tracing::debug!(session_id=%key.creation.session_id,error=%error,"managed boot remains closed");
            }
        }
    }
    Ok(())
}
pub(crate) fn spawn(state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(error) = run_once(&state).await {
                tracing::warn!(error=%error,"managed boot scan failed");
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    })
}
/// Explicit trusted recovery action. Durable capture identity precedes host I/O.
pub async fn capture(
    state: &SharedState,
    request: ManagedCaptureRequest,
) -> Result<ManagedCaptureAuthority, ApiError> {
    let row = state
        .services
        .meta
        .get_managed_boot(&request.key.creation)
        .await?
        .ok_or_else(conflict)?;
    if row.request.key != request.key {
        return state
            .services
            .meta
            .reserve_managed_capture(&request)
            .await?
            .ok_or_else(|| conflict().into());
    }
    if row.phase != ManagedBootPhase::Interrupted {
        state
            .services
            .meta
            .advance_managed_boot(&request.key, row.revision, ManagedBootAdvance::Interrupt)
            .await?;
    }
    if let Some(authority) = state
        .services
        .meta
        .reserve_managed_capture(&request)
        .await?
    {
        return Ok(authority);
    }
    let authority = host_rpc(
        restore_rpc_timeout(),
        state.services.host.capture_managed_boot(request.clone()),
    )
    .await?;
    if authority.request != request {
        return Err(conflict().into());
    }
    state
        .services
        .meta
        .record_managed_capture(&authority)
        .await?;
    Ok(authority)
}

#[cfg(test)]
#[path = "managed_boot_tests.rs"]
pub(crate) mod tests;
