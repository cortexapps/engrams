//! The NBD slot/generation/park/un-pause DEVICE-PLANE world model (ADR 0098
//! Phase 2 P7 + R6/#806), extracted for reuse across BOTH host simulators.
//!
//! `engram-dst-host`'s [`SimHost`](crate::SimHost) owns a usize-indexed slot
//! vector wired to the disk backend; the coordinator↔host boundary simulator
//! (`engram-dst-cosim`, rung 2, #784) keys its sandboxes by the
//! coordinator-minted [`SandboxId`] and reuses the finalize/reconcile flows.
//! Both need the SAME device-serving model — the generation counter, the real
//! [`NbdSlotAllocator`], the `served_by`/`kernel_owner`/`parked` state, the
//! `guest_holds_device` proof-of-death input (#806), and the transitions the
//! roll → register-rehydrate → stale-sweep → un-pause family drives over it.
//!
//! This module is that shared model. It holds ONLY the device-plane
//! bookkeeping; every DECISION is delegated to the REAL pure verdicts in
//! [`engram_host_core::reattach`] ([`sweep_verdict`], [`resume_data_plane_served`],
//! [`is_local_survivor_candidate`]) and every slot lease comes from the REAL
//! [`NbdSlotAllocator`]. The disk backend is a SEPARATE concern the owning host
//! rebuilds after a [`DevicePlane::serve`] reports a newly-served device — the
//! plane never touches it.
//!
//! Keyed by [`SandboxId`] so both hosts share one impl. Iteration that feeds a
//! decision is `BTreeMap`-ordered (determinism, ADR 0098 D5).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use engram_core::SandboxId;
use engram_host_agent::disk_daemon::{NbdSlot, NbdSlotAllocator};
use engram_host_core::{
    is_local_survivor_candidate, resume_data_plane_served, sweep_verdict, DeviceHolder,
    PidLiveness, SweepAction,
};

/// One sandbox's device-plane slot — the Flow B fields, lifted verbatim out of
/// `SimHost`'s `SandboxSlot`. The DISK tier (`backend`/`published_ref`) is the
/// owning host's concern and lives there, not here.
pub struct DeviceSlot {
    /// The `/dev/nbdN` this sandbox's rootfs is served over.
    pub nbd_device: PathBuf,
    /// The current generation's real slot lease from [`DevicePlane::nbd_pool`].
    /// `Some` while this generation serves the device; forgotten (not
    /// released) on a roll, exactly like `abandon_for_shutdown`.
    pub lease: Option<NbdSlot>,
    /// The host-agent generation whose serve socket the kernel serves this
    /// device with (`RECONFIGURE`d). `None` = unserved (post-roll,
    /// pre-rehydrate, or after a stale-sweep DISCONNECT). "served by THIS
    /// generation" (the un-pause gate) is `served_by == Some(generation)`.
    pub served_by: Option<u32>,
    /// The generation the kernel records as the device's configuring owner
    /// (`/sys/block/nbdN/pid` stand-in). SURVIVES a roll — only a DISCONNECT
    /// clears it — so the stale-binding sweep probes it against liveness.
    pub kernel_owner: Option<u32>,
    /// Rung-2 parked (evicting-shaped: FC paused, VM resident). The 731df805
    /// class was a parked survivor whose device the sweep disconnected.
    pub parked: bool,
    /// R6/#806: does the FC guest process still hold this sandbox's `/dev/nbdN`
    /// node open? The guest is a SEPARATE process from the host-agent, so it
    /// SURVIVES a host-agent roll and keeps reading its rootfs across the gap —
    /// the world-side twin of the prod `device_has_live_holder` proc-scan. A
    /// live holder ⇒ [`DeviceHolder::LiveHolder`], driving the sweep to PARK
    /// (never DISCONNECT) a dead-owner device the rehydrate missed. `false`
    /// models a genuinely-gone guest (FC crashed/destroyed) ⇒ `NoHolder`.
    pub guest_holds_device: bool,
}

impl DeviceSlot {
    /// A fresh sandbox's device, served + owned by `generation`, guest
    /// resident.
    pub fn fresh(nbd_device: PathBuf, lease: NbdSlot, generation: u32) -> Self {
        Self {
            nbd_device,
            lease: Some(lease),
            served_by: Some(generation),
            kernel_owner: Some(generation),
            parked: false,
            guest_holds_device: true,
        }
    }
}

/// The outcome of a [`DevicePlane::serve`] — tells the owning host whether the
/// device was NEWLY served (so it must rebuild the disk backend if the RAM
/// died) or was already served this generation (idempotent no-op).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServeOutcome {
    /// The device is now served by this generation (claimed + RECONFIGUREd);
    /// the caller must ensure the disk backend exists.
    NewlyServed,
    /// Already served by this generation — nothing to do (mirrors
    /// `rehydrate_sandbox`'s `nbd_sandboxes` presence check).
    AlreadyServed,
}

/// The shared NBD device-plane model for one simulated host-agent generation.
pub struct DevicePlane {
    /// The current process generation (a pid stand-in). Bumped on every
    /// [`roll`](DevicePlane::roll); the successor's rehydrate re-serves under
    /// the new generation while the kernel devices retain the old (dead) owner
    /// until re-served or swept.
    pub generation: u32,
    /// The REAL portable slot allocator for THIS generation. A roll replaces it
    /// (a fresh host-agent process builds a fresh pool; the kernel device
    /// bindings survive in [`DeviceSlot::kernel_owner`]).
    pub nbd_pool: Arc<NbdSlotAllocator>,
    /// The pool's device universe size (constant across generations).
    pub nbd_capacity: u32,
    /// Per-sandbox device slots, keyed by the (coordinator-minted, or
    /// SimHost-literal) [`SandboxId`].
    pub slots: BTreeMap<SandboxId, DeviceSlot>,
    /// Spare-device leases held for the slot-accounting exercise (dropped on a
    /// roll). Separate from per-sandbox `lease`s so the accounting oracle sees
    /// every held slot.
    spare_leases: Vec<NbdSlot>,
}

impl DevicePlane {
    /// A gen-1 plane over a pool of `capacity` devices (none warmed).
    pub fn new(capacity: u32) -> Self {
        Self {
            generation: 1,
            nbd_pool: NbdSlotAllocator::with_capacity(capacity, 0),
            nbd_capacity: capacity,
            slots: BTreeMap::new(),
            spare_leases: Vec::new(),
        }
    }

    /// Claim `device` on the current-generation pool and register a fresh slot
    /// for `id`, served + owned by this generation. Used at sandbox creation.
    pub async fn create_slot(&mut self, id: SandboxId, device: PathBuf) -> Result<(), String> {
        let lease = self.nbd_pool.claim(&device).await.ok_or_else(|| {
            format!(
                "create_slot: claim of {} failed (not free?)",
                device.display()
            )
        })?;
        let slot = DeviceSlot::fresh(device, lease, self.generation);
        self.slots.insert(id, slot);
        Ok(())
    }

    /// Insert a slot whose device lease was ALREADY claimed off this plane's
    /// pool (the concurrency split some hosts need: claim the device with a
    /// cloned `nbd_pool` handle OUTSIDE a lock, then commit the slot under a
    /// brief synchronous lock). The lease must have come from
    /// [`nbd_pool`](DevicePlane::nbd_pool).
    pub fn insert_served_slot(&mut self, id: SandboxId, device: PathBuf, lease: NbdSlot) {
        self.slots
            .insert(id, DeviceSlot::fresh(device, lease, self.generation));
    }

    /// The lowest `/dev/nbdN` ordinal (`N < capacity`) not currently owned by
    /// any slot — a free device path for a new sandbox. `None` when the
    /// universe is exhausted (the small-world capacity should preclude this).
    pub fn next_free_device(&self) -> Option<PathBuf> {
        let in_use: std::collections::BTreeSet<PathBuf> =
            self.slots.values().map(|s| s.nbd_device.clone()).collect();
        (0..self.nbd_capacity)
            .map(|n| PathBuf::from(format!("/dev/nbd{n}")))
            .find(|p| !in_use.contains(p))
    }

    /// Forget a sandbox's device slot entirely (a coordinator-driven destroy /
    /// a terminal finalize). Dropping the [`DeviceSlot`] drops its lease,
    /// releasing the device back to the pool.
    pub fn remove_slot(&mut self, id: SandboxId) {
        self.slots.remove(&id);
    }

    /// A host-agent process death is a ROLL (ADR 0098 P7): the successor comes
    /// up as a fresh GENERATION with a fresh slot pool. The kernel `/dev/nbdN`
    /// devices, and the OWNER pid the kernel recorded for them, SURVIVE (the
    /// pod-roll survival contract); only the in-process serve sockets + slot
    /// leases die. So `served_by` clears while `kernel_owner` persists at the
    /// now-DEAD generation until the successor re-serves (RECONFIGURE) or the
    /// stale-binding sweep disconnects it. `parked` + `guest_holds_device`
    /// persist — the FC VM (a separate process) stays resident across the roll.
    pub fn roll(&mut self) {
        self.generation += 1;
        for slot in self.slots.values_mut() {
            slot.lease = None;
            slot.served_by = None;
        }
        self.spare_leases.clear();
        self.nbd_pool = NbdSlotAllocator::with_capacity(self.nbd_capacity, 0);
    }

    /// Is `id` a resident survivor whose device is bound to a DEAD (prior)
    /// generation — i.e. it needs rehydrating?
    pub fn is_resident_survivor(&self, id: SandboxId) -> bool {
        self.slots
            .get(&id)
            .and_then(|s| s.kernel_owner)
            .is_some_and(|g| g < self.generation)
    }

    /// Is `id`'s device served by THIS generation (the un-pause gate's serve
    /// predicate / the sweep's claimed-in-pool gate)?
    pub fn served_by_current(&self, id: SandboxId) -> bool {
        self.slots
            .get(&id)
            .is_some_and(|s| s.served_by == Some(self.generation))
    }

    /// Rung-2 PARK: the FC VM pauses but stays RESIDENT and its NBD device
    /// keeps being served by the current generation (status `evicting`-shaped)
    /// — the exact pre-condition of the 731df805 incident. A no-op on a device
    /// this generation is not currently serving.
    pub fn park(&mut self, id: SandboxId) {
        let gen = self.generation;
        if let Some(slot) = self.slots.get_mut(&id) {
            if slot.served_by == Some(gen) {
                slot.parked = true;
            }
        }
    }

    /// R6/#806: model the FC guest process genuinely dying (crash/destroy) so
    /// it no longer holds its `/dev/nbdN` node open. After this a dead-owner
    /// sweep sees `NoHolder` and a DISCONNECT is legal.
    pub fn kill_guest(&mut self, id: SandboxId) {
        if let Some(slot) = self.slots.get_mut(&id) {
            slot.guest_holds_device = false;
        }
    }

    /// Claim + serve `id`'s device on the CURRENT generation (RECONFIGURE).
    /// Idempotent: a device already served this generation returns
    /// [`ServeOutcome::AlreadyServed`] with no pool interaction (mirrors
    /// `rehydrate_sandbox`'s presence check). The device is FREE in the fresh
    /// post-roll pool, so `claim`'s fast path takes it with no retry (safe
    /// under paused tokio). The caller rebuilds the disk backend on
    /// [`ServeOutcome::NewlyServed`].
    pub async fn serve(&mut self, id: SandboxId) -> Result<ServeOutcome, String> {
        if self.served_by_current(id) {
            return Ok(ServeOutcome::AlreadyServed);
        }
        let device = self
            .slots
            .get(&id)
            .map(|s| s.nbd_device.clone())
            .ok_or_else(|| format!("serve: unknown sandbox {id}"))?;
        let lease =
            self.nbd_pool.claim(&device).await.ok_or_else(|| {
                format!("serve: claim of {} failed (not free?)", device.display())
            })?;
        let g = self.generation;
        let slot = self.slots.get_mut(&id).expect("slot present (just read)");
        slot.lease = Some(lease);
        slot.served_by = Some(g);
        slot.kernel_owner = Some(g);
        Ok(ServeOutcome::NewlyServed)
    }

    /// The #739 local-survivor candidate predicate for `id`, over the REAL pure
    /// [`is_local_survivor_candidate`]: a live (resident-survivor), unserved,
    /// session-bound record is a local-rehydrate candidate. `has_session` is
    /// the caller's knowledge (every sim record carries a bound session).
    pub fn is_local_survivor_candidate(&self, id: SandboxId, has_session: bool) -> bool {
        let live = self.is_resident_survivor(id);
        let served = self.served_by_current(id);
        is_local_survivor_candidate(live, served, has_session)
    }

    /// The stale-binding sweep (ADR 0098 P7 + R6/#806): a dead-owner device is
    /// DISCONNECTed only with **proof of death** (no live process holds its
    /// node open), driven over the REAL [`sweep_verdict`] with the holder
    /// input. Only devices NOT served this generation are reached — the
    /// `served_by == current` gate mirrors the driver's free-in-pool
    /// `try_claim` gate, so a re-served survivor is never swept (the 731df805
    /// protection). A dead-owner device whose guest still holds it open PARKs
    /// (left kernel-bound, RECONNECTABLE) instead of severing (#769 gap A).
    ///
    /// Returns the sandboxes whose device was PARKed this tick (a live guest
    /// blocked the disconnect) — the `sweep-blocked-live-holder` set the
    /// oracle asserts against.
    pub fn stale_sweep_tick(&mut self) -> Vec<SandboxId> {
        let gen = self.generation;
        let mut parked_live = Vec::new();
        for (id, slot) in self.slots.iter_mut() {
            if slot.served_by == Some(gen) {
                continue; // claimed ⇒ not free ⇒ the sweep skips it
            }
            let liveness = match slot.kernel_owner {
                None => PidLiveness::NoPid,
                Some(g) if g == gen => PidLiveness::SelfPid,
                Some(_) => PidLiveness::Dead,
            };
            let holder = if slot.guest_holds_device {
                DeviceHolder::LiveHolder
            } else {
                DeviceHolder::NoHolder
            };
            match sweep_verdict(liveness, holder) {
                SweepAction::Disconnect => slot.kernel_owner = None,
                SweepAction::Park => parked_live.push(*id),
                SweepAction::NotStuck => {}
            }
        }
        parked_live
    }

    /// Un-pause `id` (rung-cancel resume). The **un-pause data-plane gate** (ADR
    /// 0098 P7): the guest is un-paused only when its rootfs device is served by
    /// THIS generation; otherwise the gate fires (over the REAL
    /// [`resume_data_plane_served`]) and the guest stays parked, routed to
    /// `evict_local → resume` — it NEVER lands on a dead data plane. Returns
    /// `true` if un-paused, `false` if the gate fired (or nothing to un-pause).
    pub fn unpause(&mut self, id: SandboxId) -> bool {
        let served = self.served_by_current(id);
        let Some(slot) = self.slots.get_mut(&id) else {
            return false;
        };
        if !slot.parked {
            return false;
        }
        if resume_data_plane_served(true, served) {
            slot.parked = false;
            true
        } else {
            false
        }
    }

    /// Is `id` currently parked?
    pub fn is_parked(&self, id: SandboxId) -> bool {
        self.slots.get(&id).is_some_and(|s| s.parked)
    }

    /// Does a live guest still hold `id`'s device node open?
    pub fn guest_holds_device(&self, id: SandboxId) -> bool {
        self.slots.get(&id).is_some_and(|s| s.guest_holds_device)
    }

    /// The kernel-recorded owner generation for `id`'s device (`None` = no
    /// binding / disconnected).
    pub fn kernel_owner(&self, id: SandboxId) -> Option<u32> {
        self.slots.get(&id).and_then(|s| s.kernel_owner)
    }

    /// Exercise the REAL slot allocator: `try_claim` a spare device (Free →
    /// Claimed), holding the lease. `try_claim` is single-shot (no retry), so
    /// it is hang-free under paused tokio and its synchronous reserved-bit
    /// protocol guarantees NO double-claim — a device a prior claim holds
    /// returns `None` (benign). Held leases feed the slot-accounting oracle.
    pub async fn slot_claim(&mut self, device: PathBuf) {
        if let Some(lease) = self.nbd_pool.try_claim(&device).await {
            self.spare_leases.push(lease);
        }
    }

    /// Release the oldest held spare lease (Claimed → Free), exercising the
    /// allocator's release path.
    pub fn slot_populate_tick(&mut self) {
        if !self.spare_leases.is_empty() {
            let _ = self.spare_leases.remove(0);
        }
    }

    /// The count of slot leases this generation holds (per-sandbox served
    /// devices + spare leases) — the "claimed + parked" term of the
    /// slot-accounting identity `free + warm + held == capacity`.
    pub fn leases_held(&self) -> usize {
        let sandbox_leases = self.slots.values().filter(|s| s.lease.is_some()).count();
        sandbox_leases + self.spare_leases.len()
    }
}
