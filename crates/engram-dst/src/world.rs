//! The simulated world: replicas, hosts, and the world-truth the
//! invariant checkers compare against.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use engram_coordinator::{AppState, CoordinatorConfig, HostRegistry, Services};
use engram_core::traits::{BlobStorage as _, Entropy as _, HostClient, SessionFence};
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxProbe, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{HostId, SandboxError, SandboxId, SessionId};
use engram_sim::{SimClock, SimEntropy, SimMetadataStore};
use parking_lot::Mutex;

/// World-side truth for one simulated host. The coordinator's beliefs
/// (host rows, session bindings) are compared against THIS by the
/// invariant checkers — the whole point of keeping it outside SimMeta.
#[derive(Debug, Default)]
pub struct SimHostState {
    pub up: bool,
    /// Asymmetric partition: the host is UP and serves RPCs, but its
    /// heartbeats never land (the dead-host detector's hardest case —
    /// the issue-#231 probe exists exactly for this).
    pub heartbeats_partitioned: bool,
    /// The inverse of the heartbeat partition: heartbeats land but RPC
    /// verbs STALL (ADR 0098 coverage-gap G1, the #743 op-wedge class).
    /// `Some(d)` = every RPC on this host sleeps `d` of virtual time
    /// before answering — above the verb's `op_deadline` this is the
    /// 03e6535e wedge (the within-step heartbeat keeps the op row fresh
    /// while the dispatch never returns; only the tokio-timer deadline
    /// unwedges it), below it a plain delay. Replaces the fail-fast
    /// `rpc_partitioned` flag that was toggled but never read.
    pub rpc_hang: Option<std::time::Duration>,
    /// sandbox -> owning session (as told to us via create's spec).
    pub sandboxes: BTreeMap<SandboxId, Option<SessionId>>,
}

/// A mutating host-verb's WORLD-side effect (ADR 0098 R2). Every
/// state-changing `SimHostClient` verb produces one of these instead of
/// touching `SimHostState.sandboxes` inline. In the default (inline)
/// mode the effect applies immediately — byte-for-byte the pre-R2 world,
/// so Calm seeds are unchanged. When the target host is in the deferred
/// set (a Chaos fault window), the effect is queued for a later
/// scheduler step to deliver / drop / duplicate / reorder — which is
/// what makes RPC loss/reorder/dup and a replica crash BETWEEN the store
/// commit (the ack the coordinator already got) and the world effect a
/// reachable state (the audit's finding #2/#4).
#[derive(Debug, Clone)]
pub enum Effect {
    /// `create` / `restore` / `restore_base_for_session`: a new sandbox
    /// appears on the host, ownership learned later at bind time.
    Create {
        sandbox: SandboxId,
    },
    Destroy {
        sandbox: SandboxId,
    },
    Bind {
        session: SessionId,
        sandbox: SandboxId,
    },
    Unbind {
        session: SessionId,
    },
}

#[derive(Debug, Clone)]
pub struct QueuedEffect {
    pub host: HostId,
    pub effect: Effect,
}

// --- ADR 0108 E: the harness-attach plane ---------------------------

/// One simulated harness connection's phase (ADR 0108 E: `Dialing →
/// Attached`, with a nudge dropping either back into `Dialing`).
///
/// Production splits "VM ready" from "harness attached": the harness
/// dials its vsock connection 50–200 ms AFTER `start_agent` returns, and
/// the host hub answers `send_prompt` `NotFound` until the registration
/// lands. Pre-0108 the sim collapsed the two into one atomic condition
/// (`send_prompt` succeeded whenever the host was up), so the 2026-07-31
/// boot/attach race class was structurally invisible.
#[derive(Debug, Clone)]
pub enum HarnessPhase {
    /// `start_agent` (or a resume-time re-dial) was issued; the dial
    /// completes at `due`. `swallowed` = the attach frame was lost (the
    /// ADR 0108 vsock black-hole fault): the dial NEVER completes until
    /// a nudge reschedules it. `serial` identifies THIS dial — a nudge
    /// replaces it, so an unchanged serial proves an in-flight dial was
    /// left alone.
    Dialing {
        serial: u64,
        due: tokio::time::Instant,
        swallowed: bool,
    },
    /// Registered on the hub: `send_prompt` succeeds.
    Attached,
    /// ADR 0108 A8 (prod 7eddce62, the fourth stall shape): the link is
    /// DEAD but the hub has not reaped the handle yet. The hub view
    /// ([`SimHostWorld::harness_attached`]) still reports it attached,
    /// so a heartbeat counts it in the `attached` set and `send_prompt`
    /// is ACCEPTED (Ok) — but the socket has no reader: the accepted
    /// prompt is never recorded, so the pump never echoes `run_started`
    /// for it, and no Idle announcement occurs. The hub notices at
    /// `reap_due` (the scheduled reap): from then on the handle is gone
    /// — `harness_attached` is false, `send_prompt` answers `NotFound`,
    /// and the normal recovery path (reattach → dial → Attached)
    /// applies. Only a long-pause severance enters here (the fault
    /// knob [`SimHostWorld::sever_attach_stale`]); destroy/crash
    /// severances stay immediate.
    Stale { reap_due: tokio::time::Instant },
}

/// The attach plane's shared state. A separate mutex from `hosts`; lock
/// order is always hosts → attach (never nested the other way).
#[derive(Debug)]
pub struct AttachPlane {
    next_serial: u64,
    /// Per-sandbox harness phase, tagged with the owning host. Entries
    /// for destroyed sandboxes are purged on destroy/crash and validated
    /// again at dial completion (a dead machine's dial dies with it).
    harness: BTreeMap<SandboxId, (HostId, HarnessPhase)>,
    /// The scheduled dial latency — the attach-delayed-by-N fault knob.
    /// Default mirrors the observed 50–200 ms production lag.
    delay: std::time::Duration,
    /// How long a [`HarnessPhase::Stale`] handle survives before the
    /// hub notices the dead link and reaps it — a knob like `delay`.
    /// The window must be BOUNDED: an unreaped stale handle would
    /// silence both `NotFound` recovery and the A8 heartbeat repair
    /// forever. Sampled at severance time.
    reap_window: std::time::Duration,
    /// Hosts whose NEWLY scheduled dials are swallowed (the fault is
    /// sampled at schedule time; healing the flag does not revive an
    /// already-swallowed dial — only a nudge does, as in production).
    swallowed_hosts: BTreeSet<HostId>,
    /// Prompts an Attached harness accepted (`send_prompt` Ok), awaiting
    /// the sim guest's `run_started` echo (drained by the scheduler's
    /// harness pump).
    delivered: Vec<(SandboxId, Option<SessionId>, String)>,
    /// Dial nudges per sandbox (every `start_agent`/SIGUSR1-equivalent
    /// bumps this) — observability for the no-destructive-reattach
    /// assertions in the pinned regression scenarios.
    nudges: BTreeMap<SandboxId, u32>,
    /// When true, a completed attach ANNOUNCES Idle through the real
    /// ingestion route (the ADR 0108 A3 attach signal). Scenario opt-in,
    /// default false: swarm-wide announcements mark every Active session
    /// soft-idle-eligible (`classify` keys on `last_event_kind ==
    /// harness_idle`), which multiplies evict/resume churn across long
    /// chaos seeds far past their step budgets AND exposes the two
    /// coordinator wedges documented at `Step::Prompt` — re-enable
    /// swarm-wide when those land (ADR 0108 E follow-up).
    announce: bool,
}

impl Default for AttachPlane {
    fn default() -> Self {
        Self {
            next_serial: 0,
            harness: BTreeMap::new(),
            delay: std::time::Duration::from_millis(200),
            reap_window: std::time::Duration::from_secs(5),
            swallowed_hosts: BTreeSet::new(),
            delivered: Vec::new(),
            nudges: BTreeMap::new(),
            announce: false,
        }
    }
}

/// Deferred host-effects, keyed by a monotonic serial so delivery order
/// (and the seeded reorder fault) is deterministic. `deferred` is the set
/// of hosts whose verbs currently enqueue instead of applying inline.
#[derive(Debug, Default)]
pub struct EffectQueue {
    next_serial: u64,
    pending: BTreeMap<u64, QueuedEffect>,
    deferred: BTreeSet<HostId>,
}

#[derive(Debug)]
pub struct SimHostWorld {
    pub hosts: Mutex<BTreeMap<HostId, SimHostState>>,
    pub effects: Mutex<EffectQueue>,
    /// ADR 0108 E: the harness-attach plane (dial scheduling + hub
    /// registration + the accepted-prompt echo queue).
    pub attach: Mutex<AttachPlane>,
    /// This world's PRIVATE blob "bucket" — a deterministic in-memory
    /// [`MemBlobStorage`](engram_sim::MemBlobStorage) (ADR 0098 D5 +
    /// determinism-audit item 7). Every replica's `Services.blob`/`chunk_store`
    /// and this host's capture path share THIS `Arc` (coordinator + host share
    /// one bucket, as in prod), but two DIFFERENT worlds — sibling seeds under
    /// `nextest --workspace`, or successive seeds in the swarm binary — get
    /// distinct values, so cross-world residue is structurally impossible.
    ///
    /// It replaces the pre-R4 per-world `LocalBlobStorage` over a `TempDir`
    /// (#791/#795). That backed blob ops with real `tokio::fs` I/O, which the
    /// blocking thread pool serviced off the current thread — and on the sim's
    /// `start_paused` runtime an awaited off-thread op makes the runtime "idle",
    /// so the paused clock AUTO-ADVANCES by however long the real filesystem
    /// took. Virtual time became a function of real disk latency, host load, and
    /// platform (green on macOS, invariant on Linux; #795's per-world TempDir
    /// killed cross-world *contamination* but left this real-time leak, which
    /// #797's extra blob-HEAD await re-exposed). The in-memory store completes
    /// every op synchronously in-poll — no blocking-pool handoff, no idle
    /// window, no clock auto-advance mid-I/O — and its `BTreeMap` gives sorted
    /// `list_prefix` (the GCS/S3 contract) for free.
    blob: Arc<engram_sim::MemBlobStorage>,
}

impl Default for SimHostWorld {
    fn default() -> Self {
        Self {
            hosts: Mutex::new(BTreeMap::new()),
            effects: Mutex::new(EffectQueue::default()),
            attach: Mutex::new(AttachPlane::default()),
            blob: Arc::new(engram_sim::MemBlobStorage::new()),
        }
    }
}

impl SimHostWorld {
    /// This world's isolated in-memory blob store, shared by the coordinator
    /// replicas and the host capture path within the world (as in prod), never
    /// across worlds. Returned as the concrete `Arc` so callers can hand it to
    /// both `Services.blob` and `ChunkStore` (each coerces to `dyn BlobStorage`).
    pub fn blob(&self) -> Arc<engram_sim::MemBlobStorage> {
        self.blob.clone()
    }

    fn with_host<R>(
        &self,
        id: HostId,
        f: impl FnOnce(&mut SimHostState) -> Result<R, SandboxError>,
    ) -> Result<R, SandboxError> {
        let mut hosts = self.hosts.lock();
        let host = hosts.get_mut(&id).ok_or(SandboxError::HostLost)?;
        if !host.up {
            // The same typed retryable error the real transport surfaces
            // when a host stops answering.
            return Err(SandboxError::Unavailable(format!("sim: host {id} is down")));
        }
        f(host)
    }

    /// The RPC-time liveness gate for a MUTATING verb: a down/unknown host
    /// fails exactly as `with_host` did (so a create against a dead host
    /// still surfaces `Unavailable`/`HostLost`), but the world mutation is
    /// separated out into an [`Effect`] recorded via [`record_effect`].
    fn require_up(&self, id: HostId) -> Result<(), SandboxError> {
        let hosts = self.hosts.lock();
        let host = hosts.get(&id).ok_or(SandboxError::HostLost)?;
        if !host.up {
            return Err(SandboxError::Unavailable(format!("sim: host {id} is down")));
        }
        Ok(())
    }

    /// Record a verb's world-effect. Inline unless the host is deferred.
    fn record_effect(&self, host: HostId, effect: Effect) {
        let deferred = {
            let mut q = self.effects.lock();
            if q.deferred.contains(&host) {
                let serial = q.next_serial;
                q.next_serial += 1;
                q.pending.insert(
                    serial,
                    QueuedEffect {
                        host,
                        effect: effect.clone(),
                    },
                );
                true
            } else {
                false
            }
        };
        if !deferred {
            self.apply_effect(host, &effect);
        }
    }

    /// Apply one effect to world truth. A down/absent host swallows it —
    /// the machine that would have held the sandbox is gone (a create
    /// effect never resurrects a restarted host's cleared VM set; a stale
    /// destroy is a harmless no-op).
    fn apply_effect(&self, host: HostId, effect: &Effect) {
        // ADR 0108 E: a destroyed VM takes its harness connection (and
        // any in-flight dial) with it. Done BEFORE the hosts lock —
        // lock order is hosts → attach, never nested the other way.
        if let Effect::Destroy { sandbox } = effect {
            self.attach.lock().harness.remove(sandbox);
        }
        let mut hosts = self.hosts.lock();
        let Some(h) = hosts.get_mut(&host) else {
            return;
        };
        if !h.up {
            return;
        }
        match effect {
            Effect::Create { sandbox } => {
                h.sandboxes.entry(*sandbox).or_insert(None);
            }
            Effect::Destroy { sandbox } => {
                h.sandboxes.remove(sandbox);
            }
            Effect::Bind { session, sandbox } => {
                if let Some(owner) = h.sandboxes.get_mut(sandbox) {
                    *owner = Some(*session);
                }
            }
            Effect::Unbind { session } => {
                for owner in h.sandboxes.values_mut() {
                    if *owner == Some(*session) {
                        *owner = None;
                    }
                }
            }
        }
    }

    // --- Scheduler-driven queue control (ADR 0098 R2) -----------------

    /// Toggle a host's deferred window. While on, that host's mutating
    /// verbs enqueue; the committed-but-unapplied window opens.
    pub fn set_deferred(&self, host: HostId, on: bool) {
        let mut q = self.effects.lock();
        if on {
            q.deferred.insert(host);
        } else {
            q.deferred.remove(&host);
        }
    }

    pub fn clear_deferred(&self) {
        self.effects.lock().deferred.clear();
    }

    pub fn pending_serials(&self) -> Vec<u64> {
        self.effects.lock().pending.keys().copied().collect()
    }

    pub fn pending_len(&self) -> usize {
        self.effects.lock().pending.len()
    }

    /// Deliver every pending effect in serial (causal) order and clear the
    /// queue — the normal, fault-free delivery step and the quiescence
    /// flush.
    pub fn deliver_in_order(&self) {
        let drained: Vec<QueuedEffect> = {
            let mut q = self.effects.lock();
            std::mem::take(&mut q.pending).into_values().collect()
        };
        for qe in drained {
            self.apply_effect(qe.host, &qe.effect);
        }
    }

    /// Deliver pending effects in the caller-supplied (seeded-shuffled)
    /// serial order — the REORDER fault. Serials absent from `order` are
    /// dropped; the scheduler always passes a full permutation.
    pub fn deliver_shuffled(&self, order: &[u64]) {
        let pending: BTreeMap<u64, QueuedEffect> = {
            let mut q = self.effects.lock();
            std::mem::take(&mut q.pending)
        };
        for serial in order {
            if let Some(qe) = pending.get(serial) {
                self.apply_effect(qe.host, &qe.effect);
            }
        }
    }

    /// Drop one queued effect without applying it — the LOSS fault.
    pub fn drop_pending(&self, serial: u64) -> bool {
        self.effects.lock().pending.remove(&serial).is_some()
    }

    /// Apply one queued effect an EXTRA time while leaving it queued — the
    /// DUPLICATE fault. Our effects are map-keyed and hence idempotent, so
    /// this exercises the coordinator's tolerance of a re-delivered verb
    /// rather than corrupting world truth.
    pub fn duplicate_pending(&self, serial: u64) {
        let qe = self.effects.lock().pending.get(&serial).cloned();
        if let Some(qe) = qe {
            self.apply_effect(qe.host, &qe.effect);
        }
    }

    /// A crashed/restarted host severs its in-flight RPCs: pending effects
    /// targeting it die with the machine (never resurrected onto the
    /// cleared VM set).
    pub fn drop_host_pending(&self, host: HostId) {
        self.effects.lock().pending.retain(|_, qe| qe.host != host);
    }

    // --- ADR 0108 E: harness-attach plane control ---------------------

    /// Set the dial latency — the attach-delayed-by-N fault. Applies to
    /// dials scheduled AFTER the call.
    pub fn set_attach_delay(&self, delay: std::time::Duration) {
        self.attach.lock().delay = delay;
    }

    /// Set the stale-handle reap window — how long a severed link stays
    /// advertised on the hub before the reap. Applies to severances
    /// armed AFTER the call.
    pub fn set_attach_reap_window(&self, window: std::time::Duration) {
        self.attach.lock().reap_window = window;
    }

    /// The long-pause severance fault (prod 7eddce62): kill the harness
    /// link of an ATTACHED sandbox while the hub keeps advertising the
    /// handle. Returns `true` if the handle went Stale. Only `Attached`
    /// can go stale — a dial has no established link to sever, and a
    /// second severance must not extend the reap window. Destroy/crash
    /// severances keep their immediate behavior (`apply_effect` /
    /// `drop_host_harness`) — they never route through Stale.
    pub fn sever_attach_stale(&self, sandbox: SandboxId) -> bool {
        let mut plane = self.attach.lock();
        let reap_due = tokio::time::Instant::now() + plane.reap_window;
        match plane.harness.get_mut(&sandbox) {
            Some((_, phase)) => match phase {
                HarnessPhase::Attached => {
                    *phase = HarnessPhase::Stale { reap_due };
                    true
                }
                HarnessPhase::Dialing { .. } | HarnessPhase::Stale { .. } => false,
            },
            None => false,
        }
    }

    /// Reap due Stale handles: the hub notices the dead link and drops
    /// the registration. From this moment `harness_attached` is false
    /// and `send_prompt` answers `NotFound` — the disagreement window
    /// the A8 heartbeat repair keys on OPENS here (prod: the six
    /// disagreement heartbeats fired only after the hub noticed).
    /// Driven from the scheduler's per-step pump, so the reap lands
    /// deterministically at a step boundary.
    pub fn reap_due_stale_handles(&self) {
        let now = tokio::time::Instant::now();
        self.attach
            .lock()
            .harness
            .retain(|_, (_, phase)| match phase {
                HarnessPhase::Stale { reap_due } => *reap_due > now,
                HarnessPhase::Dialing { .. } | HarnessPhase::Attached => true,
            });
    }

    /// Whether this sandbox's handle is Stale (link dead, hub not yet
    /// reaped) — scenario observability.
    pub fn harness_stale(&self, sandbox: SandboxId) -> bool {
        match self.attach.lock().harness.get(&sandbox) {
            Some((_, HarnessPhase::Stale { .. })) => true,
            Some((_, HarnessPhase::Attached)) | Some((_, HarnessPhase::Dialing { .. })) | None => {
                false
            }
        }
    }

    /// Opt into the attach-completion Idle announcement (the A3 signal)
    /// — see the `announce` field note for why the swarm defaults off.
    pub fn set_attach_announce(&self, on: bool) {
        self.attach.lock().announce = on;
    }

    /// Whether completed attaches announce Idle (read by the pump).
    pub fn attach_announce(&self) -> bool {
        self.attach.lock().announce
    }

    /// Arm/heal the swallowed-attach fault on one host. Sampled at dial
    /// SCHEDULE time: healing the flag never revives an already-swallowed
    /// dial — only a nudge does (production: vsock does not retransmit;
    /// the lost attach frame needs a SIGUSR1 re-dial).
    pub fn set_attach_swallowed(&self, host: HostId, on: bool) {
        let mut plane = self.attach.lock();
        if on {
            plane.swallowed_hosts.insert(host);
        } else {
            plane.swallowed_hosts.remove(&host);
        }
    }

    /// The SIGUSR1-equivalent nudge every `start_agent` issues (and a
    /// restore's captured-warm harness issues for itself): an in-flight
    /// dial is DROPPED and restarted; an established connection is
    /// dropped and re-dialed. The fresh dial samples the host's CURRENT
    /// swallow flag and the current delay.
    pub fn nudge_attach(&self, host: HostId, sandbox: SandboxId) {
        let mut plane = self.attach.lock();
        let serial = plane.next_serial;
        plane.next_serial += 1;
        let due = tokio::time::Instant::now() + plane.delay;
        let swallowed = plane.swallowed_hosts.contains(&host);
        plane.harness.insert(
            sandbox,
            (
                host,
                HarnessPhase::Dialing {
                    serial,
                    due,
                    swallowed,
                },
            ),
        );
        *plane.nudges.entry(sandbox).or_default() += 1;
    }

    /// Complete every due, un-swallowed dial: flip it to `Attached` and
    /// return `(sandbox, owner-session)` in dial-schedule order so the
    /// scheduler's pump can announce Idle through the real ingestion
    /// route (the ADR 0108 A3 attach signal). A dial whose host/sandbox
    /// died since is discarded — the connection died with the machine.
    pub fn complete_due_attaches(&self) -> Vec<(SandboxId, Option<SessionId>)> {
        let now = tokio::time::Instant::now();
        // Pass 1 (attach lock): collect due dials. Exhaustive match — a
        // new phase must be handled here, not silently skipped.
        let due: Vec<(u64, SandboxId, HostId)> = {
            let plane = self.attach.lock();
            let mut due: Vec<(u64, SandboxId, HostId)> = plane
                .harness
                .iter()
                .filter_map(|(sandbox, (host, phase))| match *phase {
                    HarnessPhase::Dialing {
                        serial,
                        due,
                        swallowed,
                    } => (!swallowed && due <= now).then_some((serial, *sandbox, *host)),
                    // A Stale handle has no dial in flight; its only
                    // exits are the reap and a nudge.
                    HarnessPhase::Attached | HarnessPhase::Stale { .. } => None,
                })
                .collect();
            due.sort_by_key(|(serial, _, _)| *serial);
            due
        };
        if due.is_empty() {
            return Vec::new();
        }
        // Pass 2 (hosts lock): validate against world truth + resolve the
        // owner for the Idle announcement.
        let validated: Vec<(u64, SandboxId, Option<Option<SessionId>>)> = {
            let hosts = self.hosts.lock();
            due.into_iter()
                .map(|(serial, sandbox, host)| {
                    let owner = hosts
                        .get(&host)
                        .filter(|h| h.up)
                        .and_then(|h| h.sandboxes.get(&sandbox).copied());
                    (serial, sandbox, owner)
                })
                .collect()
        };
        // Pass 3 (attach lock): commit. Re-check the serial — a nudge
        // between passes replaced the dial and must win.
        let mut completed = Vec::new();
        let mut plane = self.attach.lock();
        for (serial, sandbox, owner) in validated {
            let current = plane.harness.get(&sandbox).map(|(_, p)| p.clone());
            // Exhaustive — a new phase must decide here, not silently
            // lose the committing dial.
            let still_this_dial = match current {
                Some(HarnessPhase::Dialing { serial: s, .. }) => s == serial,
                Some(HarnessPhase::Attached) | Some(HarnessPhase::Stale { .. }) | None => false,
            };
            if !still_this_dial {
                continue;
            }
            match owner {
                Some(owner) => {
                    if let Some((_, phase)) = plane.harness.get_mut(&sandbox) {
                        *phase = HarnessPhase::Attached;
                    }
                    completed.push((sandbox, owner));
                }
                // Host down or sandbox gone: the dial dies.
                None => {
                    plane.harness.remove(&sandbox);
                }
            }
        }
        completed
    }

    /// The HUB's advertised view: does it hold a registered handle for
    /// this sandbox? A `Stale` handle IS attached here — the hub has
    /// not noticed the dead link yet, which is the whole 7eddce62
    /// shape: the heartbeat `attached` set carries it, `send_prompt`
    /// is accepted, and the attach-disagreement oracle stays quiet
    /// until the reap opens the window (as in prod, where the alarm
    /// fired only after the hub noticed).
    pub fn harness_attached(&self, sandbox: SandboxId) -> bool {
        match self.attach.lock().harness.get(&sandbox) {
            Some((_, HarnessPhase::Attached)) | Some((_, HarnessPhase::Stale { .. })) => true,
            Some((_, HarnessPhase::Dialing { .. })) | None => false,
        }
    }

    /// The current in-flight dial's serial (None when attached, stale,
    /// or absent).
    pub fn dial_serial(&self, sandbox: SandboxId) -> Option<u64> {
        match self.attach.lock().harness.get(&sandbox) {
            Some((_, HarnessPhase::Dialing { serial, .. })) => Some(*serial),
            Some((_, HarnessPhase::Attached)) | Some((_, HarnessPhase::Stale { .. })) | None => {
                None
            }
        }
    }

    /// ADR 0108 A8: the two sandbox sets a host heartbeat carries —
    /// `running` (the backend's list) and `attached` (the hub's
    /// advertised handles, [`Self::harness_attached`]'s view). A Stale
    /// handle is IN `attached`: the disagreement `harness_desync::
    /// run_once` repairs opens only at reap. Hosts lock is taken and
    /// released BEFORE the attach lock (lock order: hosts → attach).
    pub fn heartbeat_sets(&self, host: HostId) -> (BTreeSet<SandboxId>, BTreeSet<SandboxId>) {
        let running: BTreeSet<SandboxId> = {
            let hosts = self.hosts.lock();
            hosts
                .get(&host)
                .map(|h| h.sandboxes.keys().copied().collect())
                .unwrap_or_default()
        };
        let attached = running
            .iter()
            .copied()
            .filter(|s| self.harness_attached(*s))
            .collect();
        (running, attached)
    }

    /// How many dial nudges (`start_agent` calls / SIGUSR1-equivalents)
    /// this sandbox has received.
    pub fn attach_nudges(&self, sandbox: SandboxId) -> u32 {
        self.attach
            .lock()
            .nudges
            .get(&sandbox)
            .copied()
            .unwrap_or(0)
    }

    /// The hub relay gate for `send_prompt`: exhaustive over the phase,
    /// so a new phase is a compile-checked delivery decision here.
    /// `Attached` accepts and records the prompt for the pump's
    /// `run_started` echo (the confirming event that retires the outbox
    /// row). `Stale` ACCEPTS (the hub still advertises the handle —
    /// `Ok` into a socket with no reader, prod 7eddce62) but records
    /// NOTHING: no echo ever follows. Dialing/absent answer `NotFound`.
    fn hub_accept_prompt(
        &self,
        sandbox: SandboxId,
        owner: Option<SessionId>,
        prompt_id: String,
    ) -> Result<(), SandboxError> {
        let mut plane = self.attach.lock();
        match plane.harness.get(&sandbox) {
            Some((_, HarnessPhase::Attached)) => {
                plane.delivered.push((sandbox, owner, prompt_id));
                Ok(())
            }
            Some((_, HarnessPhase::Stale { .. })) => Ok(()),
            Some((_, HarnessPhase::Dialing { .. })) | None => Err(SandboxError::NotFound),
        }
    }

    /// Drain the accepted prompts awaiting their `run_started` echo.
    pub fn take_delivered_prompts(&self) -> Vec<(SandboxId, Option<SessionId>, String)> {
        std::mem::take(&mut self.attach.lock().delivered)
    }

    /// A dead host machine severs its harness plane: every connection and
    /// in-flight dial on it dies (crash AND restart).
    pub fn drop_host_harness(&self, host: HostId) {
        self.attach.lock().harness.retain(|_, (h, _)| *h != host);
    }
}

/// A per-host `HostClient` over the shared world. Registered into each
/// replica's `HostRegistry` under its host id, exactly like a real
/// remote host's client.
#[derive(Debug)]
pub struct SimHostClient {
    pub host_id: HostId,
    pub world: Arc<SimHostWorld>,
    pub entropy: Arc<SimEntropy>,
}

impl SimHostClient {
    /// The G1 RPC hang/delay fault: stall this verb by the host's
    /// configured `rpc_hang` before touching the world. The lock is
    /// released BEFORE the sleep; on the paused clock the sleep resolves
    /// deterministically — either auto-advance reaches it (a delay) or
    /// the caller's `op_deadline` timeout fires first and drops this
    /// future mid-sleep (the wedge). Never `future::pending()` — an
    /// un-timed caller would deadlock the run-step-to-completion
    /// scheduler.
    async fn maybe_hang(&self) {
        let hang = {
            let hosts = self.world.hosts.lock();
            hosts.get(&self.host_id).and_then(|h| h.rpc_hang)
        };
        if let Some(d) = hang {
            tokio::time::sleep(d).await;
        }
    }
}

#[async_trait]
impl HostClient for SimHostClient {
    async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        self.maybe_hang().await;
        // Draw the id BEFORE the liveness gate so the entropy stream is
        // identical to the pre-effect-queue world even on a down-host
        // failure (Calm determinism).
        let id = SandboxId::from(self.entropy.uuid());
        self.world.require_up(self.host_id)?;
        // Ownership is learned at bind_session time (the spec is a
        // template, not a binding — see sandbox.rs's type docs).
        self.world
            .record_effect(self.host_id, Effect::Create { sandbox: id });
        Ok(id)
    }

    async fn destroy(&self, id: SandboxId, _fence: SessionFence) -> Result<(), SandboxError> {
        self.maybe_hang().await;
        self.world.require_up(self.host_id)?;
        self.world
            .record_effect(self.host_id, Effect::Destroy { sandbox: id });
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        self.maybe_hang().await;
        self.world
            .with_host(self.host_id, |h| Ok(h.sandboxes.keys().copied().collect()))
    }

    async fn probe_sandbox(&self, id: SandboxId) -> Result<SandboxProbe, SandboxError> {
        self.maybe_hang().await;
        self.world.with_host(self.host_id, |h| {
            let known = h.sandboxes.contains_key(&id);
            Ok(SandboxProbe {
                known_to_backend: known,
                process_alive: known,
                control_alive: Some(known),
            })
        })
    }

    async fn exec_stream(
        &self,
        _id: SandboxId,
        _cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        self.maybe_hang().await;
        // The D5 workload never execs; guest behavior is out of scope
        // (ADR 0098 non-goal). Fail loudly if a driver starts doing it.
        Err(SandboxError::Unsupported(
            "sim: exec is not modeled (ADR 0098 non-goal)".into(),
        ))
    }

    async fn snapshot(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        self.maybe_hang().await;
        let entropy = self.entropy.clone();
        // Draw every id under the host lock (existence-gated, deterministic
        // ordering: snapshot, then disk, then memory manifest), then drop
        // the lock before touching blob storage.
        let (snapshot_id, disk_manifest, memory_manifest) =
            self.world.with_host(self.host_id, |h| {
                if !h.sandboxes.contains_key(&id) {
                    return Err(SandboxError::NotFound);
                }
                let snapshot_id = engram_core::SnapshotId::from(entropy.uuid());
                let disk_manifest = ManifestRef {
                    manifest_id: entropy.uuid(),
                    version: 1,
                };
                let memory_manifest = ManifestRef {
                    manifest_id: entropy.uuid(),
                    version: 1,
                };
                Ok((snapshot_id, disk_manifest, memory_manifest))
            })?;
        // FIDELITY (issue #790): a real evict capture uploads its chunked
        // disk+memory manifests AND the portable FC state.bin/sidecar.json
        // to blob storage BEFORE the coordinator records the row — so the
        // coordinator's honest `verify_snapshot_recoverable` (a real
        // `blob.head` on the manifest keys) and resume-time
        // `snapshot_artifacts_present` (state.bin/sidecar HEADs) both pass and
        // the row lands `recoverable = true`. The old manifest-less metadata
        // made every capture record `recoverable = false`, so an idle-evicted
        // session reached `Idle` with no recoverable durable copy and tripped
        // the snapshot-safety oracle. We back the refs with REAL blobs in the
        // SAME per-world store the coordinator reads, mirroring
        // engram-dst-host's ledger discipline (model the artifact, never fake
        // the flag).
        let blob = self.world.blob();
        for key in [
            disk_manifest.storage_key(),
            memory_manifest.storage_key(),
            engram_chunk_store::snapshot_blob::state_blob_key(snapshot_id),
            engram_chunk_store::snapshot_blob::sidecar_blob_key(snapshot_id),
        ] {
            blob.put(&key, bytes::Bytes::from_static(b"sim"))
                .await
                .map_err(|e| SandboxError::Snapshot(format!("sim: blob put {key}: {e}")))?;
        }
        // Round-trip through serde: every other Option field is
        // `#[serde(default)]`, so this JSON IS the canonical metadata — no
        // hand-listing the remaining fields.
        let meta: SnapshotMetadata = serde_json::from_value(serde_json::json!({
            "id": snapshot_id,
            "size_bytes": 0,
            "created_at": chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
            "image_version": "sim",
            "disk_manifest": disk_manifest,
            "memory_manifest": memory_manifest,
        }))
        .expect("minimal snapshot metadata");
        Ok(meta)
    }

    async fn restore(
        &self,
        _metadata: SnapshotMetadata,
        _fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        self.maybe_hang().await;
        let id = SandboxId::from(self.entropy.uuid());
        self.world.require_up(self.host_id)?;
        self.world
            .record_effect(self.host_id, Effect::Create { sandbox: id });
        // ADR 0108 E: a snapshot restore resumes a captured-warm harness
        // (ADR 0037) which re-dials on the vsock epoch bump — schedule
        // the dial now, independent of any later `start_agent` (which
        // would nudge/replace it harmlessly).
        self.world.nudge_attach(self.host_id, id);
        Ok(id)
    }

    async fn start_agent(
        &self,
        id: SandboxId,
        _agent: AgentSpec,
        _policy: SessionEgressPolicy,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        self.maybe_hang().await;
        self.world.with_host(self.host_id, |h| {
            if h.sandboxes.contains_key(&id) {
                Ok(())
            } else {
                Err(SandboxError::NotFound)
            }
        })?;
        // ADR 0108 E: a successful `start_agent` begins the harness dial
        // (fresh spawn) or SIGUSR1-nudges an existing one (drop +
        // re-dial). Registration is NOT synchronous with the verb — it
        // lands after the sim-scheduled dial delay.
        self.world.nudge_attach(self.host_id, id);
        Ok(())
    }

    /// The boot pipeline's actual restore leg (fresh create = restore
    /// from the image's base snapshot). Allocates a sandbox on this
    /// host — the default impl errors, which left every sim boot
    /// retrying to Failed.
    async fn restore_base_for_session(
        &self,
        _metadata: SnapshotMetadata,
        _session_env: std::collections::HashMap<String, String>,
        _selected_mounts: Vec<engram_core::types::sandbox::AuxRoDrive>,
        _fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        self.maybe_hang().await;
        let id = SandboxId::from(self.entropy.uuid());
        self.world.require_up(self.host_id)?;
        self.world
            .record_effect(self.host_id, Effect::Create { sandbox: id });
        Ok(id)
    }

    async fn guest_ip(&self, _id: SandboxId) -> Option<std::net::Ipv4Addr> {
        None
    }

    async fn bind_session(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
        _binding_epoch: u64,
    ) {
        self.maybe_hang().await;
        if self.world.require_up(self.host_id).is_ok() {
            self.world.record_effect(
                self.host_id,
                Effect::Bind {
                    session: session_id,
                    sandbox: sandbox_id,
                },
            );
        }
    }

    async fn unbind_session(&self, session_id: SessionId) {
        self.maybe_hang().await;
        if self.world.require_up(self.host_id).is_ok() {
            self.world.record_effect(
                self.host_id,
                Effect::Unbind {
                    session: session_id,
                },
            );
        }
    }

    async fn send_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        _text: String,
        _mode: Option<String>,
    ) -> Result<(), SandboxError> {
        self.maybe_hang().await;
        // ADR 0108 E: the hub gate. The VM existing is NOT enough — the
        // relay is decided by the harness phase (`hub_accept_prompt`):
        // an unknown sandbox and an un-dialed harness answer `NotFound`
        // (the ambiguity the coordinator's attach grace, A4, resolves);
        // a Stale handle answers Ok into a dead socket (A8).
        let owner = self
            .world
            .with_host(self.host_id, |h| match h.sandboxes.get(&sandbox_id) {
                Some(owner) => Ok(*owner),
                None => Err(SandboxError::NotFound),
            })?;
        self.world.hub_accept_prompt(sandbox_id, owner, prompt_id)
    }
}

/// One coordinator replica: a full real `AppState` over the SHARED
/// SimMeta. Crash = drop it; restart = rebuild (ADR 0047 statelessness,
/// exercised directly).
pub struct Replica {
    pub state: Option<engram_coordinator::state::SharedState>,
    /// This replica's view of wall-clock time — a SimClock over the
    /// same paused tokio base, with its own (fault-mutable) skew.
    pub clock: Arc<SimClock>,
}

pub struct SimWorld {
    pub clock: Arc<SimClock>,
    pub entropy: Arc<SimEntropy>,
    pub meta: Arc<SimMetadataStore>,
    pub host_world: Arc<SimHostWorld>,
    pub host_ids: Vec<HostId>,
    pub replicas: Vec<Replica>,
}

impl SimWorld {
    pub fn new(seed: u64, replicas: usize, hosts: usize) -> Self {
        let clock = SimClock::new();
        let entropy = Arc::new(SimEntropy::seeded(seed));
        let meta = SimMetadataStore::new(clock.clone(), Arc::new(SimEntropy::seeded(seed ^ 0xE)));
        let host_world = Arc::new(SimHostWorld::default());
        let host_ids: Vec<HostId> = (0..hosts)
            .map(|i| {
                // Deterministic, readable host ids.
                HostId::from(::uuid::Uuid::from_u128(0x0D57_0000 + i as u128))
            })
            .collect();
        {
            let mut hw = host_world.hosts.lock();
            for id in &host_ids {
                hw.insert(
                    *id,
                    SimHostState {
                        up: true,
                        ..SimHostState::default()
                    },
                );
            }
        }
        let mut world = Self {
            clock,
            entropy,
            meta,
            host_world,
            host_ids,
            replicas: Vec::new(),
        };
        for _ in 0..replicas {
            let clock = SimClock::new();
            let state = world.build_replica_with_clock(clock.clone());
            world.replicas.push(Replica {
                state: Some(state),
                clock,
            });
        }
        world
    }

    /// Build a fresh replica over the shared authority — also the
    /// restart path after a crash fault. The replica keeps its own
    /// clock (with any accumulated skew) across restarts — machines
    /// keep their clocks when processes die.
    pub fn build_replica_with_clock(
        &self,
        clock: Arc<SimClock>,
    ) -> engram_coordinator::state::SharedState {
        let registry = Arc::new(HostRegistry::new(
            self.meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        for id in &self.host_ids {
            registry.register(
                *id,
                Arc::new(SimHostClient {
                    host_id: *id,
                    world: self.host_world.clone(),
                    entropy: self.entropy.clone(),
                }) as Arc<dyn HostClient>,
            );
        }
        let blob = self.host_world.blob();
        let services = Services {
            meta: self.meta.clone(),
            host: registry.clone() as Arc<dyn HostClient>,
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "sim:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: blob.clone(),
            chunk_store: engram_chunk_store::ChunkStore::new(blob),
            materialize_dir: None,
            clock,
            entropy: self.entropy.clone(),
        };
        Arc::new(AppState::new_with_registry(
            CoordinatorConfig::default(),
            services,
            registry,
        ))
    }
}

impl SimWorld {
    /// Seed an enabled image (+ its base snapshot row) so the boot
    /// pipeline's prepare leg resolves it exactly like production.
    pub fn seed_enabled_image(&self, uri: &str) {
        use engram_core::traits::{Clock as _, Entropy as _, MetadataStore as _};
        let now = self.clock.now_utc();
        let base_snapshot_id = engram_core::SnapshotId::from(self.entropy.uuid());
        let snapshot: engram_core::types::snapshot::SnapshotRecord =
            serde_json::from_value(serde_json::json!({
                "id": base_snapshot_id,
                "session_id": null,
                "host_id": null,
                "image_version": uri,
                "size_bytes": 0,
                "created_at": now,
                "last_accessed_at": now,
                "recoverable": true,
            }))
            .expect("minimal base snapshot row");
        let image = engram_core::types::EnabledImage {
            id: self.entropy.uuid(),
            image_uri: uri.to_string(),
            // FIDELITY (R3 #722): the workload reserves `mem_budget_mib: 2048`
            // / `cpu_budget_vcpus: 2` at create (scheduler.rs), so the enabled
            // image MUST resolve to the SAME budget — in production
            // `reserve_and_persist_create` and `resolve_cold_boot_spec` both
            // derive from `ImageConfig::resolved_memory_mib`/`_vcpus`, so
            // create-reserve == resume-resolve by construction. The old
            // `name = "sim"`-only config left memory at DEFAULT_MEMORY_MIB
            // (4096) while create reserved 2048: a resume's cold-boot spec then
            // needed 4096 where the queued row recorded 2048, so the
            // queue-scanner precheck (2048, fits) and the resume verb's own gate
            // (4096, no fit) disagreed forever — an Idle↔Queued livelock the
            // faithful-host resume-capacity fix surfaced (seed 142). vCPUs
            // already default to DEFAULT_VCPUS (2) = the create budget; pin
            // memory to close the gap.
            image_config: toml::from_str(
                "name = \"sim\"\n[resources]\nsuggested_memory_mib = 2048\nsuggested_vcpus = 2\n",
            )
            .expect("sim image config"),
            oci_defaults: Default::default(),
            manifest_digest: "sha256:sim".into(),
            disk_manifest: None,
            base_snapshot_id: Some(base_snapshot_id),
            base_snapshot_disk_manifest: Some(engram_core::types::manifest::ManifestRef {
                manifest_id: self.entropy.uuid(),
                version: 1,
            }),
            base_snapshot_memory_manifest: Some(engram_core::types::manifest::ManifestRef {
                manifest_id: self.entropy.uuid(),
                version: 1,
            }),
            last_refreshed_at: now,
            created_at: now,
            updated_at: None,
            soft_deleted_at: None,
        };
        // Synchronous seeding on a fresh world — block_on is fine here
        // (no runtime nesting: called before the sim loop starts)... but
        // we ARE inside the test's runtime, so spawn-and-wait instead.
        let meta = self.meta.clone();
        futures_block(async move {
            meta.record_snapshot(snapshot)
                .await
                .expect("seed base snapshot");
            meta.upsert_enabled_image(image)
                .await
                .expect("seed enabled image");
        });
    }
}

/// Poll a future to completion on the CURRENT thread without a nested
/// runtime — valid because SimMeta never actually suspends.
fn futures_block<F: std::future::Future<Output = ()>>(f: F) {
    let mut f = Box::pin(f);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    for _ in 0..1024 {
        if let std::task::Poll::Ready(()) = f.as_mut().poll(&mut cx) {
            return;
        }
    }
    panic!("seed future did not complete synchronously");
}
