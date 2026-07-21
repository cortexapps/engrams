//! `/dev/nbdN` slot allocator with a warm pool.
//!
//! The Linux kernel exposes a fixed number of NBD devices
//! (`/dev/nbd0`..`/dev/nbd{nbds_max-1}`) sized by the `nbds_max`
//! module parameter at `modprobe` time. The host-agent treats them
//! as a pool: each FC sandbox grabs a slot at restore time, holds it
//! for the VM's lifetime, and returns it on destroy.
//!
//! ## Why a warm pool (ADR 0049)
//!
//! The original allocator held an explicit free-list of device paths
//! and, on every `acquire()`, scanned the list calling a `/sys`
//! `stat` per slot to skip kernel-busy devices, sleeping-and-retrying
//! when the (small) pool was exhausted. Under a same-image burst —
//! many sessions restoring at once on one host — that pool (16 slots
//! in prod) exhausted instantly, every `acquire()` did O(slots)
//! syscalls, and restores blocked in the sleep-retry loop holding
//! their reservation. The fleet wedged.
//!
//! This allocator mirrors E2B's `DevicePool`:
//!
//! - **Bitset over `0..max`.** Slots are tracked by a reserved bit,
//!   not an enumerated path list — so the universe can be thousands
//!   of devices cheaply (`modprobe nbd nbds_max=4096`).
//! - **Warm pool.** A background populator keeps up to `warm_target`
//!   *pre-validated* free slots ready in a queue. `acquire()` is an
//!   O(1) pop with **zero hot-path syscalls** — the populator already
//!   paid the `/sys` free-check.
//! - **Sturdier free-check.** A device is free iff `/sys/block/nbdN/pid`
//!   is absent AND `/sys/block/nbdN/size == 0` (two signals, like
//!   E2B), so a half-torn-down device is never handed out.
//!
//! ## Wiring
//!
//! Production builds the pool from the kernel's `nbds_max` via
//! [`build_from_kernel`]; the chart sets `modprobe nbd
//! nbds_max=<N>` and the matching `ENGRAM_NBD_MAX_SLOTS` /
//! `ENGRAM_NBD_WARM_SLOTS`. The FC integration tests build a
//! restricted pool over one specific device via [`NbdSlotAllocator::from_paths`].

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::Mutex;

/// Hard ceiling on the slot universe, bounding the bitset regardless
/// of what `nbds_max` the kernel module loaded with. ADR 0049 / E2B
/// parity — 4096 is "more than enough" headroom for the densest host.
pub const MAX_SUPPORTED_SLOTS: u32 = 4096;

/// Default number of pre-validated slots the populator keeps warm.
/// `acquire()` is an O(1) pop off this pool. Overridable via
/// `ENGRAM_NBD_WARM_SLOTS`.
pub const DEFAULT_WARM_SLOTS: usize = 64;

/// How long the populator naps when the warm pool is already full —
/// the idle heartbeat. A released slot rejoins circulation within
/// this bound.
const POLL_IDLE: Duration = Duration::from_millis(100);
/// How long the populator naps while actively refilling (warm pool
/// below target). Short so a burst that drains the warm pool gets it
/// topped back up promptly.
const POLL_REFILL: Duration = Duration::from_millis(10);
/// Fallback re-poll interval for `acquire()` when the warm pool is
/// momentarily empty (burst outran the populator). The populator runs
/// independently; this just bounds the wait if a wakeup is missed.
const ACQUIRE_REPOLL: Duration = Duration::from_millis(20);

/// Parse the device index out of a `/dev/nbdN` path. `None` if the
/// path isn't a `nbd<digits>` device.
fn parse_nbd_index(path: &Path) -> Option<u32> {
    path.file_name()?
        .to_str()?
        .strip_prefix("nbd")?
        .parse()
        .ok()
}

/// Device path for a slot index.
fn slot_path(slot: u32) -> PathBuf {
    PathBuf::from(format!("/dev/nbd{slot}"))
}

/// `true` if any process still holds `/dev/nbdN` open — i.e.
/// `/sys/block/nbdN/holders/` is non-empty OR opening the device with
/// `O_EXCL` fails `EBUSY`. The holders dir lists block-layer holders
/// (a stacked device); the `O_EXCL` probe catches a plain `open()` from
/// a process (a Firecracker guest reading the rootfs is exactly this)
/// because the kernel refuses an `O_EXCL` open of a block device that
/// another opener already has open.
///
/// Issue #223 — DEFENSE IN DEPTH. After a cancellation-window
/// disconnect under a live FC, `NbdHandle::Drop` netlink-disconnects
/// the device: the kernel clears `/sys/block/nbdN/pid` and zeros
/// `size`, so the `pid`+`size` free-check below PASSES even though the
/// orphan FC still holds the device fd open. The next claimant would
/// then CONNECT its own backend onto a device the orphan reads/writes —
/// cross-session disk I/O. The open-count check detects exactly that
/// residual open fd, so a device an orphan still holds is never
/// warmed/handed out (it becomes claimable again only once the orphan's
/// `destroy()` closes its fd).
#[cfg(target_os = "linux")]
fn device_has_open_holder(slot: u32) -> bool {
    // 1. Block-layer holders (stacked devices). Non-empty dir = held.
    let holders_present = std::fs::read_dir(format!("/sys/block/nbd{slot}/holders"))
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    // 2. Plain process opener (the FC guest case). An `O_EXCL` open of a
    //    block device fails `EBUSY` when another opener already holds it.
    //    A successful open means no one else has it; close immediately.
    use std::os::unix::fs::OpenOptionsExt;
    let o_excl = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_EXCL)
        .open(format!("/dev/nbd{slot}"));
    let o_excl_errno = match &o_excl {
        Ok(_f) => None,
        Err(e) => e.raw_os_error(),
    };
    interpret_open_holder(holders_present, o_excl_errno)
}

/// Pure decision for [`device_has_open_holder`] (issue #223), split out
/// so the policy is unit-testable without a real block device. A device
/// is considered held if a block-layer holder is present, or the
/// `O_EXCL` open returned `EBUSY` (another opener has it). `o_excl_errno`
/// is `None` on a successful exclusive open (definitively unheld), or
/// `Some(errno)` on failure — only `EBUSY` is a positive "held" signal;
/// any other error (e.g. `ENOENT` on a sparse universe, `EACCES`) is not,
/// so the pid/size signals decide.
#[cfg(any(target_os = "linux", test))]
fn interpret_open_holder(holders_present: bool, o_excl_errno: Option<i32>) -> bool {
    if holders_present {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        o_excl_errno == Some(libc::EBUSY)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Off-Linux this helper is reached only from the unit test, which
        // passes the linux EBUSY code (16) explicitly.
        o_excl_errno == Some(16)
    }
}

/// Kernel-truth free-check: a device is free iff `/sys/block/nbdN/pid`
/// is absent AND `/sys/block/nbdN/size` reads `0` AND no process still
/// holds `/dev/nbdN` open (issue #223). The pid file is the kernel's
/// "bound to an NBD thread" signal; size catches a device whose binding
/// is mid-teardown (pid cleared, size not yet zeroed); the open-holder
/// check catches an orphan FC still reading a device whose NBD binding
/// was already disconnected (the cancellation-window cross-session
/// hazard).
///
/// On non-Linux (macOS dev) `/sys` doesn't exist, so the real probe is
/// never used there — production builds the pool only when the kernel
/// `nbds_max` is readable, and unit tests inject a fake.
fn device_is_free(slot: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        if Path::new(&format!("/sys/block/nbd{slot}/pid")).exists() {
            return false;
        }
        let size_zero = match std::fs::read_to_string(format!("/sys/block/nbd{slot}/size")) {
            Ok(s) => s.trim() == "0",
            // Size unreadable → be conservative, treat as not free so
            // the populator skips and re-checks rather than handing out
            // a device in an unknown state.
            Err(_) => false,
        };
        // Even a fully-disconnected device (pid cleared, size 0) is NOT
        // free while an orphan FC still holds its fd open — handing it
        // out would let a different session CONNECT a backend the orphan
        // keeps reading. See `device_has_open_holder` (issue #223).
        size_zero && !device_has_open_holder(slot)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = slot;
        true
    }
}

/// Injectable free-check (real `/sys` probe in prod; a fake in unit
/// tests so the allocator is exercisable off-Linux).
type FreeCheck = Arc<dyn Fn(u32) -> bool + Send + Sync>;

/// The explicit lifecycle of one `/dev/nbdN` slot (ADR 0098 Phase 2, P7).
///
/// The allocator tracks a slot's state IMPLICITLY across three
/// representations — the `reserved` bitset, the `warm` queue, and possession
/// of an [`NbdSlot`] handle (+ its `quarantined` flag). The allocator is
/// already portable and tested, so this enum + transition table do NOT rewire
/// it; they make the implicit FSM auditable (the slot-accounting oracle in the
/// host-internal simulator asserts against these states):
///
/// | State | Implicit representation |
/// |---|---|
/// | [`Free`](SlotState::Free) | `reserved == false`, not in the warm queue |
/// | [`Warm`](SlotState::Warm) | `reserved == true`, present in the warm queue (validated, ready) |
/// | [`Claimed`](SlotState::Claimed) | `reserved == true`, not warm, a live [`NbdSlot`] handle exists |
/// | [`Parked`](SlotState::Parked) | `reserved == true`, not warm, a `quarantine()`d handle dropped (bit never cleared) |
///
/// The populator's transient validation window — `reserved == true` but
/// neither warm nor leased, between `Inner::reserve_next` and `unreserve` — is
/// deliberately NOT a distinct state: it is a `Free` slot mid-transition to
/// `Warm` (or back to `Free`), and [`NbdSlotAllocator::claim`]'s retry loop
/// exists solely to survive it (a survivor grab treats
/// "reserved-but-not-warm-and-no-lease" as retryable, never terminal).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlotState {
    /// Not reserved, not warm — available for the populator or a direct claim.
    Free,
    /// Reserved and validated, waiting in the warm queue for an `acquire`.
    Warm,
    /// Reserved and leased out (a live `NbdSlot` handle is serving a sandbox).
    Claimed,
    /// Reserved and quarantined (a survivor whose rehydrate failed) — held out
    /// of circulation until the evict_local → resume ladder recovers it.
    Parked,
}

impl SlotState {
    /// Every state (exhaustiveness guard: a new variant is a compile error at
    /// the array literal and forces a decision in [`Self::can_transition_to`]).
    pub const ALL: [SlotState; 4] = [
        SlotState::Free,
        SlotState::Warm,
        SlotState::Claimed,
        SlotState::Parked,
    ];

    /// Whether the allocator can move a slot `self → to`. The legal edges,
    /// each mapped to the concrete allocator action:
    ///
    /// - `Free → Warm` — the populator validates a free slot and warms it.
    /// - `Free → Claimed` — `claim`/`try_claim` reserves a free device
    ///   directly (survivor grab / sweep), skipping the warm queue.
    /// - `Warm → Claimed` — `acquire`/`claim` pulls a validated slot out of
    ///   the warm queue.
    /// - `Claimed → Free` — the lease drops normally (`release` clears the
    ///   reserved bit).
    /// - `Claimed → Parked` — the lease is `quarantine()`d (bit stays set).
    ///
    /// [`Parked`](SlotState::Parked) is terminal: only a process restart
    /// clears the reserved bit. `Warm → Free` never happens (the populator
    /// never un-warms a validated slot; it only advances to `Claimed`).
    pub fn can_transition_to(self, to: SlotState) -> bool {
        use SlotState::{Claimed, Free, Parked, Warm};
        matches!(
            (self, to),
            (Free, Warm) | (Free, Claimed) | (Warm, Claimed) | (Claimed, Free) | (Claimed, Parked)
        )
    }
}

/// Lease handle for one `/dev/nbdN` slot. Auto-returns the slot to the
/// allocator on `Drop` — a sandbox holds one for the lifetime of its
/// NBD daemon, and the lease's drop releases the slot back into the
/// pool.
pub struct NbdSlot {
    slot: u32,
    path: PathBuf,
    allocator: Arc<NbdSlotAllocator>,
    /// When `true`, `Drop` does NOT release the slot back to the pool —
    /// the reserved bit stays set so the populator never re-warms it and
    /// no future `acquire`/`try_claim` can hand it out. Used to PARK a
    /// survivor's device whose rehydrate RECONFIGURE failed: the FC may
    /// still hold an open fd and read it, so returning the path to the
    /// general pool would let the startup stale-binding sweep DISCONNECT
    /// it (guest EIO) or hand it to an unrelated session. Recovery is via
    /// the evict_local → resume ladder, not the warm pool.
    quarantined: bool,
}

impl NbdSlot {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Consume this lease WITHOUT returning the slot to the pool: the
    /// reserved bit stays set permanently (until process restart). The
    /// rehydrate failure path uses this to park a survivor's live device
    /// out of circulation. See the `quarantined` field doc.
    ///
    /// The device is also registered in the allocator's parked set, so the
    /// startup classification barrier counts it as a TRACKED record even if
    /// every sandbox-derived record source has concurrently vanished (the
    /// 2026-07-21 false `rehydrate-unknown-device` alarm).
    pub fn quarantine(mut self) {
        self.allocator
            .parked
            .lock()
            .expect("parked set lock poisoned")
            .insert(self.path.clone());
        self.quarantined = true;
        // `self` drops here; the `quarantined` flag makes Drop a no-op,
        // leaving the reserved bit set so the slot is never re-handed-out.
    }
}

impl std::fmt::Debug for NbdSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NbdSlot").field("path", &self.path).finish()
    }
}

impl Drop for NbdSlot {
    fn drop(&mut self) {
        if self.quarantined {
            // Parked out of the pool by design — leave the reserved bit
            // set so the slot is never re-warmed or re-handed-out.
            return;
        }
        let slot = self.slot;
        let allocator = self.allocator.clone();
        // Fire-and-forget release: clears the reserved bit so the
        // populator can re-validate and re-warm the slot. Spawned so
        // Drop never blocks on the lock.
        //
        // Issue #224: `tokio::spawn` PANICS when no runtime is current.
        // During SIGTERM teardown the abandon path forgets the slot
        // (`NbdSandboxState::abandon_for_shutdown`), but a straggler
        // `NbdSandboxState` dropped the NORMAL way after `run()` returns
        // — once the runtime has begun dropping — would hit this Drop
        // with no current runtime: the panic-in-Drop aborts the whole
        // process (`SIGABRT`) instead of the clean exit the K2 contract
        // needs. Guard with `Handle::try_current()`: spawn onto the
        // runtime when one is live (the steady-state destroy path), else
        // run the release on a detached std thread (block_on a tiny
        // current-thread runtime). If the pool is dying with the process
        // the release is a no-op-in-effect, but the thread fallback is
        // panic-free either way.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    allocator.release(slot).await;
                });
            }
            Err(_) => {
                // No current runtime (process teardown). Don't panic;
                // release on a detached thread with its own minimal
                // runtime so the reserved bit still clears if we're
                // somehow not actually exiting.
                std::thread::Builder::new()
                    .name(format!("nbd-slot-release-{slot}"))
                    .spawn(move || {
                        if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                        {
                            rt.block_on(allocator.release(slot));
                        }
                    })
                    .ok();
            }
        }
    }
}

/// Reserved-slot bookkeeping. `universe` is the set of device indices
/// this pool may hand out (`0..max` in prod; a single device in FC
/// tests); `reserved[i]` tracks whether `universe[i]` is currently
/// warm-waiting or handed out.
#[derive(Debug)]
struct Inner {
    universe: Vec<u32>,
    pos_of: HashMap<u32, usize>,
    reserved: Vec<bool>,
    cursor: usize,
    reserved_count: usize,
}

impl Inner {
    /// Reserve the next free slot scanning forward from the cursor.
    /// Returns the device index, or `None` if every slot is reserved.
    fn reserve_next(&mut self) -> Option<u32> {
        let n = self.universe.len();
        if n == 0 {
            return None;
        }
        for _ in 0..n {
            let pos = self.cursor;
            self.cursor = (self.cursor + 1) % n;
            if !self.reserved[pos] {
                self.reserved[pos] = true;
                self.reserved_count += 1;
                return Some(self.universe[pos]);
            }
        }
        None
    }

    /// Reserve a SPECIFIC device index (survivor-rehydrate path).
    /// `false` if it's not in this pool or already reserved.
    fn reserve_specific(&mut self, slot: u32) -> bool {
        let Some(&pos) = self.pos_of.get(&slot) else {
            return false;
        };
        if self.reserved[pos] {
            return false;
        }
        self.reserved[pos] = true;
        self.reserved_count += 1;
        true
    }

    fn unreserve(&mut self, slot: u32) {
        if let Some(&pos) = self.pos_of.get(&slot) {
            if self.reserved[pos] {
                self.reserved[pos] = false;
                self.reserved_count -= 1;
            }
        }
    }

    /// Whether `slot` is part of this pool at all — the fast-`None`
    /// gate for [`NbdSlotAllocator::claim`], so its retry budget is
    /// only ever spent on transiently-reserved members, never on
    /// devices outside the universe.
    fn in_universe(&self, slot: u32) -> bool {
        self.pos_of.contains_key(&slot)
    }
}

/// Pool of `/dev/nbdN` device slots with a warm-pool front. Cheap to
/// clone via `Arc`.
pub struct NbdSlotAllocator {
    inner: Mutex<Inner>,
    /// Pre-validated slots ready for O(1) handout.
    warm: Mutex<VecDeque<u32>>,
    warm_target: usize,
    /// Total universe size — constant after construction, so kept out
    /// of the mutex for a lock-free `capacity()`.
    capacity: usize,
    free_check: FreeCheck,
    /// Devices parked by [`NbdSlot::quarantine`] — the allocator's record
    /// of `Parked` slots, which the reserved bitset alone cannot express
    /// (a parked slot's bits are indistinguishable from a claimed one's).
    /// The startup classification barrier reads this so a rehydrate-failed
    /// survivor's device stays a TRACKED record: 2026-07-21, a park raced a
    /// concurrent sandbox destroy, the FC-derived record set missed the
    /// device, and the `rehydrate-unknown-device` invariant cried wolf over
    /// a device this very process had just parked on purpose. A
    /// `std::sync::Mutex` (not the pool's async one): touched only by the
    /// sync `quarantine()` consume and the startup-time snapshot, never
    /// held across an await.
    parked: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
}

impl std::fmt::Debug for NbdSlotAllocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NbdSlotAllocator")
            .field("warm_target", &self.warm_target)
            .finish_non_exhaustive()
    }
}

impl NbdSlotAllocator {
    /// Production constructor: a contiguous universe `0..max_devices`,
    /// the real `/sys` free-check, `warm_target` warm slots. Spawns
    /// the background populator.
    pub fn with_capacity(max_devices: u32, warm_target: usize) -> Arc<Self> {
        let universe: Vec<u32> = (0..max_devices).collect();
        Self::build(universe, warm_target, Arc::new(device_is_free))
    }

    /// Build a pool restricted to a specific set of device paths — the
    /// FC integration tests (which reserve one real `/dev/nbdN`) and
    /// the survivor-rehydrate harness. Duplicate / non-`nbdN` paths are
    /// rejected loud. `warm_target` defaults to the universe size so a
    /// single-device test pool keeps its device warm.
    pub fn from_paths(paths: Vec<PathBuf>) -> Result<Arc<Self>, String> {
        let mut universe = Vec::with_capacity(paths.len());
        let mut seen = std::collections::HashSet::new();
        for p in &paths {
            let idx = parse_nbd_index(p)
                .ok_or_else(|| format!("not a /dev/nbdN device path: {}", p.display()))?;
            if !seen.insert(idx) {
                return Err(format!("duplicate NBD device in pool: {}", p.display()));
            }
            universe.push(idx);
        }
        let warm = universe.len();
        Ok(Self::build(universe, warm, Arc::new(device_is_free)))
    }

    fn build(universe: Vec<u32>, warm_target: usize, free_check: FreeCheck) -> Arc<Self> {
        let pos_of = universe
            .iter()
            .enumerate()
            .map(|(i, &slot)| (slot, i))
            .collect();
        let reserved = vec![false; universe.len()];
        let capacity = universe.len();
        let warm_target = warm_target.min(capacity);
        let me = Arc::new(Self {
            inner: Mutex::new(Inner {
                universe,
                pos_of,
                reserved,
                cursor: 0,
                reserved_count: 0,
            }),
            warm: Mutex::new(VecDeque::with_capacity(warm_target)),
            warm_target,
            capacity,
            free_check,
            parked: std::sync::Mutex::new(std::collections::HashSet::new()),
        });
        // The populator holds only a Weak ref so the allocator can drop
        // naturally (dropping the last Arc stops the populator on its
        // next tick).
        let weak = Arc::downgrade(&me);
        tokio::spawn(async move { populate(weak).await });
        me
    }

    /// Total slot count in this pool's universe. Constant; lock-free.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Wait for + claim a free slot. Pops a pre-validated slot off the
    /// warm pool (O(1), no syscall). If the warm pool is momentarily
    /// empty (a burst outran the populator), re-polls until one is
    /// ready — the populator refills concurrently.
    pub async fn acquire(self: &Arc<Self>) -> NbdSlot {
        loop {
            if let Some(slot) = self.warm.lock().await.pop_front() {
                return NbdSlot {
                    slot,
                    path: slot_path(slot),
                    allocator: self.clone(),
                    quarantined: false,
                };
            }
            tokio::time::sleep(ACQUIRE_REPOLL).await;
        }
    }

    /// Claim a SPECIFIC device — the survivor-rehydrate path, where the
    /// kernel already serves the device under a surviving FC and the
    /// new host-agent generation must take ownership of exactly that
    /// slot (then RECONFIGURE it) rather than acquire a fresh one.
    /// Deliberately skips the free-check: a survivor's device is busy
    /// BY DESIGN.
    ///
    /// `None` if the device isn't in this pool, or is durably reserved
    /// by another lease (pulling it out of the warm pool if it happens
    /// to be sitting there pre-validated).
    ///
    /// RETRIES across the populator's validation window: `populate`
    /// holds a slot RESERVED for the duration of its free-check before
    /// either warming it (free) or un-reserving it (busy — the survivor
    /// case). A one-shot claim landing inside that window saw
    /// "reserved" + "not warm" and wrongly concluded the device was
    /// leased — observed as a flaky survivor rehydrate (CI 2026-07-17:
    /// `rehydrate_sandbox` returned false 44 ms into a successor's
    /// startup, i.e. during the pool's very first populate pass; in
    /// prod the same race intermittently strands a survivor's disk
    /// until the evict_local → resume ladder). A genuine lease stays
    /// reserved past the whole retry budget and still returns `None`;
    /// the validation window is a sysfs read and resolves within the
    /// first retry or two.
    pub async fn claim(self: &Arc<Self>, path: &Path) -> Option<NbdSlot> {
        const CLAIM_RETRIES: u32 = 20;
        const CLAIM_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);
        let slot = parse_nbd_index(path)?;
        if !self.inner.lock().await.in_universe(slot) {
            return None;
        }
        for attempt in 0..CLAIM_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(CLAIM_RETRY_DELAY).await;
            }
            {
                let mut inner = self.inner.lock().await;
                if inner.reserve_specific(slot) {
                    return Some(NbdSlot {
                        slot,
                        path: slot_path(slot),
                        allocator: self.clone(),
                        quarantined: false,
                    });
                }
            }
            // Already reserved — it may be sitting warm (validated free,
            // not yet handed out). Pull it from the warm pool and hand it
            // out, keeping the reserved bit set.
            let mut warm = self.warm.lock().await;
            if let Some(pos) = warm.iter().position(|&s| s == slot) {
                warm.remove(pos);
                return Some(NbdSlot {
                    slot,
                    path: slot_path(slot),
                    allocator: self.clone(),
                    quarantined: false,
                });
            }
        }
        None
    }

    /// Try to claim a SPECIFIC device that the caller believes to be
    /// free — the startup stale-binding sweep. Unlike [`Self::claim`]
    /// (which deliberately grabs busy survivor devices), this RESPECTS
    /// the reserved bit: it reserves the slot ONLY if it is currently
    /// free, returning `None` if anyone else already owns it (a survivor
    /// claim, a warm slot pulled by `claim`, or a concurrent `acquire`).
    ///
    /// This closes the snapshot-vs-claim TOCTOU in the sweep: the sweep
    /// must hold the slot reserved across the (slow, sleeping) DISCONNECT
    /// so a session that wins the race for the same device can never have
    /// its live binding torn out from under it. If `try_claim` fails the
    /// sweep skips the device — someone owns it now, by definition not a
    /// stale binding.
    ///
    /// Returns `None` if the device isn't in this pool. If the slot is
    /// sitting warm (validated-free, pre-handout), it is pulled out of
    /// the warm queue and reserved so the populator can't hand it out
    /// while the sweep holds it.
    pub async fn try_claim(self: &Arc<Self>, path: &Path) -> Option<NbdSlot> {
        let slot = parse_nbd_index(path)?;
        let mut inner = self.inner.lock().await;
        if !inner.reserve_specific(slot) {
            // Already reserved by another lease (survivor / handed out /
            // pulled-warm) — someone owns it; not ours to sweep.
            return None;
        }
        // Reserved by us now. If it happened to be sitting warm, pull it
        // out of the warm queue so the populator can't hand it out (the
        // reserved bit alone wouldn't remove an already-queued entry).
        drop(inner);
        let mut warm = self.warm.lock().await;
        if let Some(pos) = warm.iter().position(|&s| s == slot) {
            warm.remove(pos);
        }
        Some(NbdSlot {
            slot,
            path: slot_path(slot),
            allocator: self.clone(),
            quarantined: false,
        })
    }

    /// Snapshot of the device paths NOT currently reserved. The
    /// post-rehydrate startup recovery scopes its stale-binding sweep
    /// to these (a survivor's claimed device is reserved, so it's never
    /// swept). With a large universe this is a one-time, off-runtime
    /// (`spawn_blocking`) scan.
    pub async fn free_paths(&self) -> Vec<PathBuf> {
        let inner = self.inner.lock().await;
        inner
            .reserved
            .iter()
            .enumerate()
            .filter(|(_, &r)| !r)
            .map(|(i, _)| slot_path(inner.universe[i]))
            .collect()
    }

    /// Return a slot to the pool. Clears the reserved bit; the
    /// populator re-validates (free-check) before re-warming, so a
    /// still-tearing-down device is skipped until truly free.
    async fn release(&self, slot: u32) {
        self.inner.lock().await.unreserve(slot);
    }

    /// Count of slots neither warm-waiting nor handed out — i.e. still
    /// available to be warmed. Cheap snapshot for heartbeat telemetry.
    pub async fn free_count(&self) -> usize {
        let inner = self.inner.lock().await;
        inner.universe.len() - inner.reserved_count
    }

    /// Count of pre-validated slots currently sitting warm. Telemetry.
    pub async fn warm_count(&self) -> usize {
        self.warm.lock().await.len()
    }

    /// Devices parked by [`NbdSlot::quarantine`] this process lifetime.
    /// The startup classification barrier folds these into its
    /// tracked-record set: a parked survivor is a device this process
    /// KNOWS about (it parked it on purpose), never an "unknown device"
    /// for the `rehydrate-unknown-device` invariant.
    pub fn parked_devices(&self) -> Vec<PathBuf> {
        self.parked
            .lock()
            .expect("parked set lock poisoned")
            .iter()
            .cloned()
            .collect()
    }
}

/// Read the kernel's `nbds_max` — the count of `/dev/nbdN` devices the
/// `nbd` module created at load. `None` if the module isn't loaded
/// (macOS dev, or a host that never ran `modprobe nbd`).
///
/// `pub(crate)` (ADR 0068): also the basis of `capabilities::probe_nbd`,
/// which reports the same value as the `CapStatus::Ok` detail so the
/// fleet view surfaces the kernel's slot ceiling without a second probe.
pub(crate) fn kernel_nbds_max() -> Option<u32> {
    std::fs::read_to_string("/sys/module/nbd/parameters/nbds_max")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Build the production NBD slot pool from the kernel's `nbds_max`,
/// capped at [`MAX_SUPPORTED_SLOTS`] and the optional
/// `ENGRAM_NBD_MAX_SLOTS` override, keeping `ENGRAM_NBD_WARM_SLOTS`
/// slots warm (default [`DEFAULT_WARM_SLOTS`]).
///
/// `None` — selecting the materialize-to-file fallback — when the nbd
/// module isn't loaded or `ENGRAM_NBD_DISABLE` is set. Must be called
/// from within a Tokio runtime (spawns the populator).
pub fn build_from_kernel() -> Option<Arc<NbdSlotAllocator>> {
    if std::env::var_os("ENGRAM_NBD_DISABLE").is_some() {
        tracing::info!("ENGRAM_NBD_DISABLE set; NBD daemon off (materialize-to-file)");
        return None;
    }
    let kernel = kernel_nbds_max()?;
    let env_cap = std::env::var("ENGRAM_NBD_MAX_SLOTS")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(MAX_SUPPORTED_SLOTS);
    let max = kernel.min(env_cap).min(MAX_SUPPORTED_SLOTS);
    if max == 0 {
        tracing::warn!("kernel nbds_max is 0; NBD daemon off (materialize-to-file)");
        return None;
    }
    let warm = std::env::var("ENGRAM_NBD_WARM_SLOTS")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_WARM_SLOTS)
        .clamp(1, max as usize);
    tracing::info!(
        slots = max,
        warm_slots = warm,
        kernel_nbds_max = kernel,
        "NBD daemon enabled (ADR 0049 warm-pool allocator); chunked rootfs serves /dev/nbdN",
    );
    Some(NbdSlotAllocator::with_capacity(max, warm))
}

/// Background populator: keeps the warm pool topped up to
/// `warm_target` with pre-validated free slots. Holds a `Weak` so the
/// allocator drops when its last `Arc` does; the loop then exits.
async fn populate(weak: Weak<NbdSlotAllocator>) {
    loop {
        let Some(me) = weak.upgrade() else {
            return;
        };

        // Top up the warm pool without blocking: reserve a slot,
        // validate it against the kernel, push it warm. Bounded to one
        // pass over the universe so a cluster of permanently-busy slots
        // (e.g. survivors, when warm_target exceeds the free count) can
        // never hot-spin — we scan at most `capacity` slots, then nap.
        let mut warmed = false;
        let mut scanned = 0usize;
        let budget = me.capacity.max(1);
        loop {
            if scanned >= budget || me.warm.lock().await.len() >= me.warm_target {
                break;
            }
            scanned += 1;
            let Some(slot) = me.inner.lock().await.reserve_next() else {
                // Every slot reserved (pool fully utilized). Nothing to
                // warm right now.
                break;
            };
            if (me.free_check)(slot) {
                me.warm.lock().await.push_back(slot);
                warmed = true;
            } else {
                // Busy (survivor / mid-teardown). Un-reserve so it isn't
                // leaked; the cursor has already advanced past it, so the
                // next pass won't immediately re-pick it.
                me.inner.lock().await.unreserve(slot);
            }
        }

        let full = me.warm.lock().await.len() >= me.warm_target;
        drop(me);
        // Nap long when steady (pool full) or stuck (nothing warmable);
        // nap short while actively refilling after a drain.
        let nap = if full || !warmed {
            POLL_IDLE
        } else {
            POLL_REFILL
        };
        tokio::time::sleep(nap).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::time::Duration;

    /// ADR 0098 P7: the auditable slot FSM. The legal edges match the
    /// allocator's concrete actions; `Parked` is terminal, and `Warm → Free`
    /// (a populator un-warming a validated slot) is never legal.
    #[test]
    fn slot_state_transition_table_matches_the_allocator() {
        use SlotState::{Claimed, Free, Parked, Warm};
        let legal = [
            (Free, Warm),      // populator warms
            (Free, Claimed),   // direct claim / try_claim on a free device
            (Warm, Claimed),   // acquire pulls from the warm queue
            (Claimed, Free),   // normal lease drop → release
            (Claimed, Parked), // quarantine() a survivor's device
        ];
        for from in SlotState::ALL {
            for to in SlotState::ALL {
                let expect = legal.contains(&(from, to));
                assert_eq!(
                    from.can_transition_to(to),
                    expect,
                    "unexpected verdict for {from:?} → {to:?}",
                );
            }
        }
        // Parked is terminal — no out-edge (only a process restart clears it).
        assert!(
            SlotState::ALL
                .iter()
                .all(|to| !Parked.can_transition_to(*to)),
            "Parked must be terminal",
        );
        // A warm slot never regresses to Free.
        assert!(
            !Warm.can_transition_to(Free),
            "the populator never un-warms"
        );
    }

    /// Build a test pool over `0..n` with an injectable busy-set so the
    /// allocator is exercisable off-Linux (no real `/sys`).
    fn test_pool(
        n: u32,
        warm: usize,
        busy: Arc<std::sync::Mutex<HashSet<u32>>>,
    ) -> Arc<NbdSlotAllocator> {
        let check_busy = busy.clone();
        NbdSlotAllocator::build(
            (0..n).collect(),
            warm,
            Arc::new(move |slot| !check_busy.lock().unwrap().contains(&slot)),
        )
    }

    async fn wait_warm(pool: &Arc<NbdSlotAllocator>, want: usize) {
        for _ in 0..200 {
            if pool.warm_count().await >= want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "warm pool never reached {want} (got {})",
            pool.warm_count().await
        );
    }

    /// CI 2026-07-17 flake → real race: `populate` holds a slot
    /// RESERVED while its free-check runs; a busy (survivor) device is
    /// reserve→check→unreserve cycled forever, and a one-shot `claim`
    /// landing inside the check window read "reserved, not warm" as "a
    /// lease owns it" and returned `None` — a successor's survivor
    /// rehydrate then strands the disk. With a single-slot pool and a
    /// deliberately SLOW busy-reporting free-check, the window
    /// dominates the timeline, so a non-retrying claim loses almost
    /// surely; the retrying claim must still win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn claim_wins_against_the_populators_validation_window() {
        let busy = Arc::new(std::sync::Mutex::new(HashSet::from([0u32])));
        let check_busy = busy.clone();
        let pool = NbdSlotAllocator::build(
            vec![0],
            1,
            Arc::new(move |slot| {
                // Hold the reserved-for-validation window open ~20 ms
                // per populate pass (prod: a sysfs read, sub-ms — the
                // exaggeration makes the pre-fix loss deterministic).
                std::thread::sleep(Duration::from_millis(20));
                !check_busy.lock().unwrap().contains(&slot)
            }),
        );
        // Let the populator get INTO a validation pass before claiming.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let claimed = pool.claim(&slot_path(0)).await;
        assert!(
            claimed.is_some(),
            "a claim racing the populator's validation window must retry \
             through it, not report the survivor's device as leased",
        );
        // And a device outside the universe still fast-fails.
        assert!(pool.claim(&slot_path(7)).await.is_none());
    }

    #[tokio::test]
    async fn populator_warms_up_to_target() {
        let busy = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let pool = test_pool(16, 4, busy);
        wait_warm(&pool, 4).await;
        // Warm pool caps at the target, not the universe.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(pool.warm_count().await, 4);
        assert_eq!(pool.capacity(), 16);
    }

    #[tokio::test]
    async fn acquire_hands_out_distinct_slots() {
        let busy = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let pool = test_pool(8, 8, busy);
        wait_warm(&pool, 8).await;
        let mut seen = HashSet::new();
        let mut held = Vec::new();
        for _ in 0..8 {
            let s = pool.acquire().await;
            assert!(
                seen.insert(s.path().to_path_buf()),
                "duplicate slot handed out"
            );
            held.push(s);
        }
        assert_eq!(pool.free_count().await, 0);
    }

    #[tokio::test]
    async fn drop_returns_slot_and_repopulates() {
        let busy = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let pool = test_pool(1, 1, busy);
        wait_warm(&pool, 1).await;
        let s = pool.acquire().await;
        assert_eq!(pool.free_count().await, 0);
        let path = s.path().to_path_buf();
        drop(s);
        // Drop is async (spawned); the populator then re-warms it.
        wait_warm(&pool, 1).await;
        let s2 = pool.acquire().await;
        assert_eq!(s2.path(), path);
    }

    #[tokio::test]
    async fn acquire_blocks_until_a_slot_frees() {
        let busy = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let pool = test_pool(1, 1, busy);
        wait_warm(&pool, 1).await;
        let first = pool.acquire().await;
        let pool2 = pool.clone();
        let task = tokio::spawn(async move { pool2.acquire().await });
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            !task.is_finished(),
            "acquire must block while the pool is full"
        );
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("blocked acquire didn't wake within 2s")
            .expect("acquire task panicked");
        assert_eq!(second.path(), Path::new("/dev/nbd0"));
    }

    #[tokio::test]
    async fn populator_skips_busy_slots() {
        // Mark slot 0 busy: the populator must warm 1 and 2, never 0.
        let busy = Arc::new(std::sync::Mutex::new(HashSet::from([0u32])));
        let pool = test_pool(3, 3, busy.clone());
        // Only 2 of 3 are free, so warm tops out at 2.
        wait_warm(&pool, 2).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(pool.warm_count().await, 2);
        let a = pool.acquire().await;
        let b = pool.acquire().await;
        let got: HashSet<_> = [a.path().to_path_buf(), b.path().to_path_buf()].into();
        assert!(
            !got.contains(Path::new("/dev/nbd0")),
            "busy slot 0 was handed out"
        );
    }

    #[tokio::test]
    async fn claim_reserves_a_specific_device() {
        // Slot 2 busy (survivor): claim must grab it even though the
        // populator won't warm it.
        let busy = Arc::new(std::sync::Mutex::new(HashSet::from([2u32])));
        let pool = test_pool(4, 4, busy);
        let claimed = pool
            .claim(Path::new("/dev/nbd2"))
            .await
            .expect("claim survivor");
        assert_eq!(claimed.path(), Path::new("/dev/nbd2"));
        // It's reserved now: acquiring the rest never yields nbd2.
        wait_warm(&pool, 3).await;
        for _ in 0..3 {
            assert_ne!(pool.acquire().await.path(), Path::new("/dev/nbd2"));
        }
    }

    #[tokio::test]
    async fn try_claim_succeeds_only_for_a_free_slot() {
        // try_claim is the sweep's TOCTOU gate: it reserves a specific
        // device ONLY if free. A second try_claim on the same device
        // must fail while the first lease is held, and succeed again
        // after it's released.
        let busy = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let pool = test_pool(4, 0, busy); // warm_target 0: no populator handouts
        let first = pool
            .try_claim(Path::new("/dev/nbd1"))
            .await
            .expect("first try_claim of a free slot");
        assert_eq!(first.path(), Path::new("/dev/nbd1"));
        assert!(
            pool.try_claim(Path::new("/dev/nbd1")).await.is_none(),
            "try_claim must fail for an already-reserved slot (someone owns it)"
        );
        // Not in the pool's universe → None.
        assert!(pool.try_claim(Path::new("/dev/nbd99")).await.is_none());
        drop(first);
        // Drop is async-spawned; let release run, then it's claimable again.
        for _ in 0..200 {
            if pool.try_claim(Path::new("/dev/nbd1")).await.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("slot never became try_claim-able again after release");
    }

    #[tokio::test]
    async fn try_claim_loses_to_a_concurrent_acquire() {
        // The exact race the startup sweep must survive: a session
        // acquires a slot that was "free at snapshot time". try_claim on
        // that same slot must then FAIL, so the sweep skips it instead of
        // disconnecting the live session's device.
        let busy = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let pool = test_pool(1, 1, busy);
        wait_warm(&pool, 1).await;
        let held = pool.acquire().await; // session wins the slot
        assert_eq!(held.path(), Path::new("/dev/nbd0"));
        assert!(
            pool.try_claim(Path::new("/dev/nbd0")).await.is_none(),
            "sweep's try_claim must lose to a live acquire → device skipped, not swept"
        );
    }

    #[tokio::test]
    async fn quarantined_slot_never_returns_to_the_pool() {
        // The rehydrate-failure park: a quarantined slot's Drop must NOT
        // release the reserved bit, so the survivor's device stays out of
        // circulation (the populator can't re-warm it; try_claim/acquire
        // can't hand it out).
        let busy = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let pool = test_pool(1, 0, busy);
        let slot = pool
            .try_claim(Path::new("/dev/nbd0"))
            .await
            .expect("claim the only slot");
        assert_eq!(pool.free_count().await, 0);
        // Consume + drop without releasing.
        slot.quarantine();
        // Give any (incorrect) spawned release a chance to run before we
        // assert the slot stayed reserved.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            pool.free_count().await,
            0,
            "quarantined slot must stay reserved (out of the pool)"
        );
        assert!(
            pool.try_claim(Path::new("/dev/nbd0")).await.is_none(),
            "quarantined device must not be re-claimable"
        );
        // …and it must be a TRACKED record for the startup classification
        // barrier (the 2026-07-21 false `rehydrate-unknown-device` alarm:
        // a parked device whose sandbox-derived records vanished was
        // reported as an unknown survivor needing an operator).
        assert_eq!(
            pool.parked_devices(),
            vec![PathBuf::from("/dev/nbd0")],
            "a quarantined device must appear in the allocator's parked set"
        );
    }

    #[tokio::test]
    async fn from_paths_rejects_non_nbd_and_duplicates() {
        assert!(NbdSlotAllocator::from_paths(vec![PathBuf::from("/dev/sda")]).is_err());
        let dup = vec![PathBuf::from("/dev/nbd0"), PathBuf::from("/dev/nbd0")];
        assert!(NbdSlotAllocator::from_paths(dup)
            .unwrap_err()
            .contains("/dev/nbd0"));
    }

    #[tokio::test]
    async fn free_paths_excludes_reserved() {
        let busy = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let pool = test_pool(4, 4, busy);
        wait_warm(&pool, 4).await;
        let held = pool.acquire().await;
        let free = pool.free_paths().await;
        assert!(
            !free.contains(&held.path().to_path_buf()),
            "held slot leaked into free_paths"
        );
    }

    /// Issue #223 — the open-holder decision used by `device_is_free`.
    /// After a cancellation-window disconnect under a live FC, the
    /// device's NBD binding is torn down (pid cleared, size 0) but the
    /// orphan FC still holds its fd open. `device_is_free` must treat
    /// THAT as not-free so a different session can never CONNECT onto a
    /// device the orphan still reads. The decision keys on EITHER a
    /// block-layer holder OR an `O_EXCL`-open `EBUSY`.
    #[test]
    fn open_holder_blocks_a_disconnected_but_still_open_device() {
        // EBUSY on the O_EXCL open (libc::EBUSY == 16): another opener
        // (the orphan FC) holds the device → held, must NOT be handed out.
        assert!(
            interpret_open_holder(false, Some(16)),
            "an O_EXCL EBUSY (device still open by an orphan FC) must read as HELD"
        );
        // A block-layer holder present → held regardless of the O_EXCL probe.
        assert!(
            interpret_open_holder(true, None),
            "a non-empty holders/ dir must read as HELD"
        );
        // Clean: no holder, exclusive open succeeded → free to hand out.
        assert!(
            !interpret_open_holder(false, None),
            "no holder + a successful O_EXCL open must read as FREE"
        );
        // A non-EBUSY open error (e.g. ENOENT=2 on a sparse universe,
        // EACCES=13) is NOT a positive held signal — the pid/size checks
        // decide, so this helper reports not-held.
        assert!(
            !interpret_open_holder(false, Some(2)),
            "ENOENT must not be misread as held"
        );
        assert!(
            !interpret_open_holder(false, Some(13)),
            "EACCES must not be misread as held"
        );
    }

    /// Issue #223 — allocator-level consequence: a device an orphan FC
    /// still holds open (the injected free-check reports it busy even
    /// though its NBD binding was disconnected) is never warmed or
    /// handed out, and rejoins the pool only once the holder clears.
    #[tokio::test]
    async fn held_device_is_never_handed_out_until_holder_clears() {
        // Slot 0 modeled as "disconnected NBD binding but orphan FC still
        // holds the fd open" — the free-check (which in prod is
        // `device_is_free`, here injected) reports it busy.
        let busy = Arc::new(std::sync::Mutex::new(HashSet::from([0u32])));
        let pool = test_pool(2, 2, busy.clone());
        // Only slot 1 is free; warm tops out at 1, slot 0 never warms.
        wait_warm(&pool, 1).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(pool.warm_count().await, 1);
        let a = pool.acquire().await;
        assert_eq!(
            a.path(),
            Path::new("/dev/nbd1"),
            "the held device (nbd0) must not be handed out"
        );
        // Orphan FC's destroy() finally closes the fd → free-check clears.
        busy.lock().unwrap().remove(&0);
        // The populator now re-warms slot 0; acquire it.
        wait_warm(&pool, 1).await;
        let b = pool.acquire().await;
        assert_eq!(
            b.path(),
            Path::new("/dev/nbd0"),
            "once the holder clears, the device rejoins the pool"
        );
    }
}
