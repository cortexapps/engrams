//! ADR 0116 (workstream B): the one place a boot's dynamic-slot contents
//! are derived.
//!
//! Every flavor of "bring a sandbox up" — fresh create, disk-only cold
//! boot, base-snapshot capture — must agree on what rides the reserved
//! `dyn_<i>` slots (ADR 0055/0062/0080: slot 0 harness, slot 1 agentd,
//! slot 2 guest-tools, slots 3.. skills). Before this module, each path
//! assembled its own slot set; the disk-only cold boot assembled NONE
//! (sentinel slots only), so a recovered guest's harness argv pointed at
//! `/opt/engram/dyn/0/…` with nothing mounted there — the 2026-08-12
//! incident (a 50-minute deterministic `spawn_harness` ENOENT loop).
//!
//! The shape: a [`SlotPlan`] names the slot contents; the async
//! `slot_plan_for_*` constructors derive it from create-time inputs or
//! from the session's PERSISTED selections (harness, skills, policy);
//! [`overlay_slots`] writes the plan's shas over a spec's reserved
//! sentinel slots; [`check_argv_slot_agreement`] is the invariant that a
//! spec can actually exec what its argv names. Flavors keep their real
//! asymmetries: capture is session-less and stays sentinel BY DESIGN
//! (the base snapshot is skill-agnostic, one per image); snapshot
//! RESUMES never re-plan slots at all (the eviction snapshot's
//! `aux_bundles` pins re-anchor host-side — see
//! `engram-sandbox-firecracker`'s restore staging).

use std::collections::HashMap;

use engram_core::types::image::ImageConfig;
use engram_core::types::sandbox::{AuxRoDrive, SandboxSpec};
use engram_core::types::Session;

use crate::error::ApiError;
use crate::state::SharedState;

/// The derived contents of the reserved dynamic-mount slots for one boot.
///
/// `harness` carries the expected harness exec path (the argv head the
/// agent spec will name) alongside the `dyn_0` mount so
/// [`check_argv_slot_agreement`] can hold at construction time, not at
/// guest boot time.
pub(crate) struct SlotPlan {
    /// `dyn_0`: the session's selected harness (ADR 0062), with the
    /// in-guest exec path its argv will point at. `None` for a dev-VM
    /// session (no harness; the slot stays sentinel).
    pub harness: Option<(String, AuxRoDrive)>,
    /// `dyn_1`: the fleet's current agentd generation (ADR 0080). `None`
    /// keeps the base snapshot's pinned generation (soft, loud-warned in
    /// the resolver) — on a cold boot the HOST stamp-resolves this slot,
    /// so a fleet that stages agentd always boots the current one.
    pub agentd: Option<AuxRoDrive>,
    /// `dyn_2`: the fleet's guest-tools generation (ttyd; ADR 0080 §D).
    /// Soft like agentd.
    pub guest_tools: Option<AuxRoDrive>,
    /// `dyn_3..`: the session's selected skills (ADR 0055).
    pub skills: Vec<AuxRoDrive>,
}

impl SlotPlan {
    /// Flatten into the `selected_mounts` vec the boot pipeline carries.
    /// Order mirrors the historical create-path assembly (skills, agentd,
    /// guest-tools, harness); slot identity rides `drive_id`, so order is
    /// cosmetic — kept stable for log/diff familiarity.
    pub(crate) fn into_mounts(self) -> Vec<AuxRoDrive> {
        let mut mounts = self.skills;
        if let Some(m) = self.agentd {
            mounts.push(m);
        }
        if let Some(m) = self.guest_tools {
            mounts.push(m);
        }
        if let Some((_, m)) = self.harness {
            mounts.push(m);
        }
        mounts
    }
}

/// The system's cold-boot `SandboxSpec` shape — a fresh kernel boot (not a
/// snapshot restore) with manifest-derived resources and the full reserved
/// slot pool carrying sentinels. Moved verbatim from `api::sessions::
/// cold_boot_spec` (ADR 0116); the capture path uses it as-is (the base
/// snapshot stays skill-agnostic — one per image, not one per
/// skill-combination), the session cold boot overlays a [`SlotPlan`].
pub(crate) fn capture_boot_spec(
    image_uri: &str,
    config: &ImageConfig,
    rootfs_manifest: Option<engram_core::types::manifest::ManifestRef>,
    // ADR 0057: network is no longer on the manifest. The caller supplies it —
    // base-snapshot capture uses allow-all (a trusted, ephemeral build step;
    // every session that later restores the snapshot gets its own policy
    // network), disk-only recovery passes the session's persisted policy network.
    network: engram_core::types::image::NetworkPolicy,
) -> SandboxSpec {
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};

    let vcpus = config.resolved_vcpus();
    let memory_mib = config.resolved_memory_mib();
    let disk_gib = config
        .resources
        .suggested_disk_gib
        .unwrap_or(crate::api::sessions::DEFAULT_DISK_GIB);
    // ADR 0112: swap is opt-in per image; 0 stays `None` so backends
    // and old sidecars see "no swap device" identically.
    let swap_mib = config.resolved_swap_mib();

    // ADR 0055: capture reserves a fixed pool of dynamic-mount slots, each
    // carrying the sentinel. Per-session creates `patch_drive` the selected
    // skills into slots in the paused restore window; the cold-boot flavor
    // instead overlays real shas pre-create (`overlay_slots`) — a fresh
    // create attaches `Some(sha)` drives verbatim.
    let aux_ro_drives = (0..AuxRoDrive::RESERVED_SLOTS)
        .map(AuxRoDrive::reserved_slot)
        .collect();

    SandboxSpec {
        image: image_uri.to_string(),
        rootfs_source: None,
        image_uri: Some(image_uri.to_string()),
        rootfs_manifest,
        cpu: CpuLimit { vcpus },
        memory: MemoryLimit {
            max_mib: memory_mib,
        },
        disk: DiskLimit { max_gib: disk_gib },
        ttl: None,
        env: config.env.clone(),
        workdir: None,
        network,
        aux_ro_drives,
        swap_mib: (swap_mib > 0).then_some(swap_mib),
    }
}

/// Write the plan's resolved shas over the spec's reserved sentinel slots,
/// keyed by `drive_id` (the slot is the device identity; the sha is the
/// content). Slots the plan doesn't name keep their sentinel.
pub(crate) fn overlay_slots(spec: &mut SandboxSpec, plan: &SlotPlan) {
    let mut by_id: HashMap<&str, &AuxRoDrive> = HashMap::new();
    for m in plan
        .skills
        .iter()
        .chain(plan.agentd.iter())
        .chain(plan.guest_tools.iter())
        .chain(plan.harness.iter().map(|(_, m)| m))
    {
        by_id.insert(m.drive_id.as_str(), m);
    }
    for slot in &mut spec.aux_ro_drives {
        if let Some(planned) = by_id.get(slot.drive_id.as_str()) {
            *slot = (*planned).clone();
        }
    }
}

/// The spec-level spawnability invariant: every argv element that points
/// into a reserved dynamic slot (`/opt/engram/dyn/<i>/…`) must be backed
/// by a drive for that slot carrying real content (`sha256 = Some`). A
/// sentinel slot under a referenced path is exactly the 2026-08-12 wedge:
/// the guest boots, agentd answers, and `spawn_harness` ENOENTs forever.
pub(crate) fn check_argv_slot_agreement(
    argv: &[String],
    drives: &[AuxRoDrive],
) -> Result<(), String> {
    for arg in argv {
        for slot in 0..AuxRoDrive::RESERVED_SLOTS {
            let mount = AuxRoDrive::slot_guest_mount(slot);
            let prefix = format!("{}/", mount.display());
            if !arg.starts_with(&prefix) {
                continue;
            }
            let id = AuxRoDrive::slot_drive_id(slot);
            let backed = drives
                .iter()
                .any(|d| d.drive_id == id && d.sha256.is_some());
            if !backed {
                return Err(format!(
                    "argv element `{arg}` points into slot {slot} (`{}`) but the spec \
                     carries no content for drive `{id}` — the guest could never exec it",
                    mount.display(),
                ));
            }
        }
    }
    Ok(())
}

/// The create path's slot assembly (moved from `prepare_inner`): the
/// caller supplies the already-resolved harness mount (create builds the
/// `AgentSpec` in the same breath) and the profile-selected skill names.
pub(crate) async fn slot_plan_for_create(
    state: &SharedState,
    harness: Option<(String, AuxRoDrive)>,
    selected_skills: &[String],
) -> Result<SlotPlan, ApiError> {
    Ok(SlotPlan {
        harness,
        agentd: crate::api::sessions::resolve_agentd_mount(state).await?,
        guest_tools: crate::api::sessions::resolve_guest_tools_mount(state).await?,
        skills: crate::api::sessions::resolve_selected_skills(state, selected_skills).await?,
    })
}

/// The recovery flavor: re-derive the slot plan from the session's
/// PERSISTED selections — the harness (ADR 0062), the RuntimeSpec's skill
/// names (ADR 0077 phase 3), re-resolved against the current fleet
/// catalog. Read errors PROPAGATE (retryable), never degrade: a cold
/// boot without its mounts boots the session's whole remaining life
/// broken, silently.
pub(crate) async fn slot_plan_for_session(
    state: &SharedState,
    session: &Session,
) -> Result<SlotPlan, ApiError> {
    let harness = if session.mode.is_dev_vm() {
        None
    } else {
        let name = state
            .services
            .meta
            .get_session_harness(session.id)
            .await
            .map_err(|e| {
                ApiError::Internal(format!(
                    "harness selection read for {} failed (retryable): {e}",
                    session.id
                ))
            })?
            .ok_or_else(|| {
                ApiError::Internal(format!(
                    "session {} is agent-mode but has no persisted harness selection — \
                     cannot rebuild its cold-boot spec",
                    session.id
                ))
            })?;
        let (_descriptor, exec, mount) =
            crate::api::sessions::resolve_harness_mount(state, &name).await?;
        Some((exec, mount))
    };
    let skill_names = state
        .services
        .meta
        .get_session_runtime_spec(session.id)
        .await
        .map_err(|e| {
            ApiError::Internal(format!(
                "runtime spec read for {} failed (retryable — refusing to cold-boot \
                 skill-less): {e}",
                session.id
            ))
        })?
        .map(|rs| rs.selected_skills)
        .unwrap_or_default();
    Ok(SlotPlan {
        harness,
        agentd: crate::api::sessions::resolve_agentd_mount(state).await?,
        guest_tools: crate::api::sessions::resolve_guest_tools_mount(state).await?,
        skills: crate::api::sessions::resolve_selected_skills(state, &skill_names).await?,
    })
}

/// ADR 0028 Fix B, rebuilt on the materializer (ADR 0116): derive the
/// disk-only recovery's cold-boot `SandboxSpec` from the session's enabled
/// image AND its persisted slot selections. `Ok(None)` when the image row
/// is gone (structural — re-enable and retry); `Err` on transient store
/// failures (the caller's retry tick is the recovery).
///
/// The returned spec satisfies [`check_argv_slot_agreement`] for the
/// harness exec by construction — enforced with an `invariant!` because a
/// spec that cannot spawn what its argv names is corruption-class: it
/// converts every downstream resume attempt into the 2026-08-12 loop.
pub(crate) async fn materialize_cold_boot(
    state: &SharedState,
    session: &Session,
) -> Result<Option<SandboxSpec>, ApiError> {
    let enabled = match state.services.meta.get_enabled_image(&session.image).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::warn!(
                session_id = %session.id,
                image = %session.image,
                "cold-boot materialize: image is not enabled; disk-only recovery unavailable",
            );
            return Ok(None);
        }
        Err(e) => {
            return Err(ApiError::Internal(format!(
                "cold-boot materialize: enabled-image lookup for {} failed (retryable): {e}",
                session.id
            )));
        }
    };
    // ADR 0080: the row carries the config as typed JSONB — no TOML parse.
    let config = enabled.effective_config();
    // ADR 0057: disk-only recovery rebuilds the session's own egress network
    // from its persisted policy (the image config carries no network). The
    // harness egress was merged into that policy at create (ADR 0063
    // addendum) — re-read, never re-merged.
    let network = match state
        .services
        .meta
        .get_session_integration_policy(session.id)
        .await
    {
        Ok(Some(json)) => engram_core::types::IntegrationPolicy::parse(&json)
            .ok()
            .flatten()
            .map(|p| p.network)
            .unwrap_or_default(),
        _ => Default::default(),
    };

    let plan = slot_plan_for_session(state, session).await?;
    let mut spec = capture_boot_spec(&session.image, &config, None, network);
    overlay_slots(&mut spec, &plan);

    if let Some((exec, _)) = &plan.harness {
        let agreement = check_argv_slot_agreement(std::slice::from_ref(exec), &spec.aux_ro_drives);
        // ADR 0099 H6 site: constructing a spec whose harness argv points at
        // an unbacked slot is corruption-class — the coordinator is stateless
        // over PG, so failing this op loudly beats shipping a guest that can
        // never spawn its harness (the 2026-08-12 wedge).
        engram_core::invariant!(
            agreement.is_ok(),
            "cold-boot spec for {} violates argv-slot agreement: {}",
            session.id,
            agreement.unwrap_err(),
        );
    }
    Ok(Some(spec))
}

/// ADR 0116 B2: everything a snapshot RESUME materializes — the agent
/// spec and the per-sandbox egress policy. **Deliberately no mount
/// field**: the eviction snapshot's `aux_bundles` pins re-anchor the
/// `dyn` slots host-side (fc restore staging hard-checks them; "resumes
/// never swap" — live guest processes hold fds into the pinned bundle),
/// and the harness egress was merged into the persisted policy at
/// create. Making those absences STRUCTURAL is the point: the pre-B2
/// shape expressed them by positionally dropping tuple fields, which is
/// how the cold-boot harness wedge survived review.
pub(crate) struct ResumeMaterials {
    pub agent: engram_core::types::sandbox::AgentSpec,
    pub policy: engram_core::types::egress::SessionEgressPolicy,
}

/// ADR 0016 §A.1.7 / ADR 0116 B2: derive the resume-shape agent spec +
/// egress policy for a session (moved verbatim from
/// `api/snapshot.rs::resolve_resume_agent_and_policy`; the manifest +
/// SecretBundle + env load once, reused for both).
///
/// The spec is **resume-shaped**: harness resolved with `prompt = None`.
/// Prompt-less is load-bearing — the initial prompt rides the harness
/// env, so a boot-shape respawn of an *exited* harness would re-inject
/// it mid-conversation; the resume shape just `--resume`s the existing
/// claude session and goes `Idle`.
///
/// Shared by `finish_resume_to_active` (a fresh post-restore sandbox)
/// and the ADR 0034 Track A desync watchdog's in-place reattach (the
/// session's existing LIVE sandbox). `None` when the manifest bundle
/// can't load (dev-VM / process backend) — callers skip the agent
/// attach, exactly as resume did before.
pub(crate) async fn materialize_snapshot_resume(
    state: &SharedState,
    session: &Session,
    sandbox_id: engram_core::SandboxId,
) -> Option<ResumeMaterials> {
    let id = session.id;
    let (resume_bundle, resume_base_env) =
        crate::api::sessions::resolve_session_env(state, session).await;
    let b = resume_bundle.as_ref()?;
    // Same split as create: agentd holds the durable session env (image env +
    // secrets + session id); the harness gets the forge broker token as a
    // per-spawn extra, from the PG-sealed row (ADR 0047) — same token across
    // coord restarts and replicas.
    let mut session_env = resume_base_env.clone();
    session_env.insert("ENGRAM_SESSION_ID".into(), id.to_string());
    // ADR 0062: the harness comes from the session's persisted selection
    // (not the baked manifest).
    let selected_harness = state
        .services
        .meta
        .get_session_harness(id)
        .await
        .ok()
        .flatten();
    let resolved = crate::api::sessions::resolve_harness(
        state,
        selected_harness.as_deref(),
        session.mode,
        id,
        session_env,
        b.config.workdir.clone(),
        // Resume: the mode was validated when its prompt was accepted.
        None,
    )
    .await
    .ok()
    .flatten()?;
    // Consumed BY NAME: only the agent spec. `resolved.mount` re-anchors
    // host-side from the snapshot's aux_bundles; `resolved.egress` is
    // already merged into the persisted policy (see [`ResumeMaterials`]).
    let mut agent = resolved.agent;
    crate::api::sessions::inject_harness_env(state, id, &mut agent.env).await;
    // ADR 0073: stamp the CURRENT epoch (this runs after the flow's
    // bind — minted for fresh-spawn resumes, unminted for live moves,
    // where the surviving harness must keep validating).
    agent.binding_epoch = state
        .services
        .meta
        .current_binding_epoch(id)
        .await
        .unwrap_or(0);
    // Rebuild the SessionEgressPolicy for `sandbox_id`. Falls back to the
    // legacy placeholder when the host has no guest IP (process backend, VZ in
    // some configs) or the IP is unparseable — same as the create path.
    let policy =
        crate::api::sessions::build_resume_egress_policy(state, id, sandbox_id, &session.image)
            .await
            .unwrap_or_else(|| crate::api::snapshot::placeholder_egress_policy(id, sandbox_id));
    Some(ResumeMaterials { agent, policy })
}

/// The placement-budget slice of the cold-boot shape, for callers that
/// need `(memory_mib, vcpus)` and nothing else (the resume verb's
/// `ScheduleContext`, ADR 0078). Best-effort by contract: `None` on any
/// miss keeps the pre-0072 capacity-soft posture — deliberately NOT
/// `materialize_cold_boot`, which does per-session slot resolution this
/// caller would throw away.
pub(crate) async fn resolve_resume_budget(
    meta: &std::sync::Arc<dyn engram_core::traits::MetadataStore>,
    session: &Session,
) -> Option<(u32, u32)> {
    match meta.get_enabled_image(&session.image).await {
        Ok(Some(row)) => {
            let config = row.effective_config();
            Some((config.resolved_memory_mib(), config.resolved_vcpus()))
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(session_id = %session.id, error = %e,
                "resume-budget lookup failed; placement falls back to capacity-soft");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sentinel_spec() -> SandboxSpec {
        capture_boot_spec(
            "ghcr.io/example/img:latest",
            &ImageConfig::default(),
            None,
            Default::default(),
        )
    }

    fn slot_mount(slot: usize, sha: &str) -> AuxRoDrive {
        AuxRoDrive {
            drive_id: AuxRoDrive::slot_drive_id(slot),
            guest_mount: AuxRoDrive::slot_guest_mount(slot),
            fs_type: "squashfs".into(),
            sha256: Some(sha.to_string()),
        }
    }

    fn harness_exec() -> String {
        format!(
            "{}/claude/bin/harness",
            AuxRoDrive::slot_guest_mount(AuxRoDrive::HARNESS_SLOT_INDEX).display()
        )
    }

    /// The 2026-08-12 incident regression, pinned at the spec level: the
    /// pre-ADR-0116 cold-boot spec (sentinel slots only) FAILS argv-slot
    /// agreement for a harness exec; the materialized overlay passes.
    #[test]
    fn cold_boot_overlay_fixes_the_sentinel_harness_wedge() {
        let mut spec = sentinel_spec();
        assert_eq!(spec.aux_ro_drives.len(), AuxRoDrive::RESERVED_SLOTS);
        let exec = vec![harness_exec()];

        // The old shape: every slot sentinel — the wedge.
        assert!(check_argv_slot_agreement(&exec, &spec.aux_ro_drives).is_err());

        let plan = SlotPlan {
            harness: Some((
                exec[0].clone(),
                slot_mount(AuxRoDrive::HARNESS_SLOT_INDEX, "sha_harness"),
            )),
            agentd: None, // host stamp resolves agentd on create — stays symbolic
            guest_tools: Some(slot_mount(AuxRoDrive::GUEST_TOOLS_SLOT_INDEX, "sha_tools")),
            skills: vec![slot_mount(AuxRoDrive::FIRST_SKILL_SLOT_INDEX, "sha_skill")],
        };
        overlay_slots(&mut spec, &plan);

        assert!(check_argv_slot_agreement(&exec, &spec.aux_ro_drives).is_ok());
        // Slot pool shape is preserved: same count, same drive ids.
        assert_eq!(spec.aux_ro_drives.len(), AuxRoDrive::RESERVED_SLOTS);
        for (i, d) in spec.aux_ro_drives.iter().enumerate() {
            assert_eq!(d.drive_id, AuxRoDrive::slot_drive_id(i));
        }
        // Planned slots carry content; unplanned slots keep the sentinel.
        let sha_of = |i: usize| spec.aux_ro_drives[i].sha256.clone();
        assert_eq!(
            sha_of(AuxRoDrive::HARNESS_SLOT_INDEX).as_deref(),
            Some("sha_harness")
        );
        assert_eq!(
            sha_of(AuxRoDrive::GUEST_TOOLS_SLOT_INDEX).as_deref(),
            Some("sha_tools")
        );
        assert_eq!(
            sha_of(AuxRoDrive::FIRST_SKILL_SLOT_INDEX).as_deref(),
            Some("sha_skill")
        );
    }

    #[test]
    fn argv_agreement_ignores_non_slot_paths_and_unreferenced_slots() {
        let spec = sentinel_spec();
        // argv that never points into a dyn slot is always fine, sentinel or not.
        let argv = vec!["/usr/bin/env".to_string(), "--flag".to_string()];
        assert!(check_argv_slot_agreement(&argv, &spec.aux_ro_drives).is_ok());
        // Empty argv trivially agrees.
        assert!(check_argv_slot_agreement(&[], &spec.aux_ro_drives).is_ok());
    }

    #[test]
    fn into_mounts_orders_like_the_historical_create_assembly() {
        let plan = SlotPlan {
            harness: Some((
                harness_exec(),
                slot_mount(AuxRoDrive::HARNESS_SLOT_INDEX, "h"),
            )),
            agentd: Some(slot_mount(AuxRoDrive::AGENTD_SLOT_INDEX, "a")),
            guest_tools: Some(slot_mount(AuxRoDrive::GUEST_TOOLS_SLOT_INDEX, "g")),
            skills: vec![
                slot_mount(AuxRoDrive::FIRST_SKILL_SLOT_INDEX, "s0"),
                slot_mount(AuxRoDrive::FIRST_SKILL_SLOT_INDEX + 1, "s1"),
            ],
        };
        let ids: Vec<String> = plan.into_mounts().into_iter().map(|m| m.drive_id).collect();
        assert_eq!(
            ids,
            vec![
                AuxRoDrive::slot_drive_id(AuxRoDrive::FIRST_SKILL_SLOT_INDEX),
                AuxRoDrive::slot_drive_id(AuxRoDrive::FIRST_SKILL_SLOT_INDEX + 1),
                AuxRoDrive::slot_drive_id(AuxRoDrive::AGENTD_SLOT_INDEX),
                AuxRoDrive::slot_drive_id(AuxRoDrive::GUEST_TOOLS_SLOT_INDEX),
                AuxRoDrive::slot_drive_id(AuxRoDrive::HARNESS_SLOT_INDEX),
            ]
        );
    }
}
