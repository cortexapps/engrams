//! `VZVirtualMachine` lifecycle wrapper.
//!
//! Builds a `VZVirtualMachineConfiguration` (kernel boot loader,
//! virtio-block rootfs, virtio-vsock, virtio-net NAT, virtio-console)
//! and exposes start/stop/pause/resume/save/restore as Rust async fns.
//!
//! All VZ method calls happen inside closures dispatched onto a
//! per-VM serial DispatchQueue — that's the threading contract VZ
//! demands, and it lets us cross the !Send / !Sync boundary that
//! `Retained<VZVirtualMachine>` carries.

use std::path::Path;
use std::sync::Mutex;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_foundation::NSFileHandle;
use objc2_foundation::{NSArray, NSError, NSString, NSURL};
use objc2_virtualization::{
    VZBootLoader, VZDiskImageStorageDeviceAttachment, VZFileHandleSerialPortAttachment,
    VZLinuxBootLoader, VZNATNetworkDeviceAttachment, VZSerialPortAttachment,
    VZSerialPortConfiguration, VZSocketDeviceConfiguration, VZStorageDeviceConfiguration,
    VZVirtioBlockDeviceConfiguration, VZVirtioConsoleDeviceSerialPortConfiguration,
    VZVirtioNetworkDeviceConfiguration, VZVirtioSocketDeviceConfiguration, VZVirtualMachine,
    VZVirtualMachineConfiguration,
};

use engram_core::types::sandbox::AuxRoDrive;
use tokio::sync::oneshot;

/// Per-VM declarative inputs. Built once at create time.
#[derive(Clone, Debug)]
pub(crate) struct VmConfig {
    pub kernel_path: std::path::PathBuf,
    pub rootfs_path: std::path::PathBuf,
    pub memory_mib: u32,
    pub vcpus: u32,
    /// Linux kernel command line. Default points root at /dev/vda
    /// (the first virtio-block device) and routes the console to hvc0.
    pub kernel_cmdline: String,
    /// ADR 0061: skill bundles to attach as read-only erofs virtio-blk
    /// drives. Only entries with `sha256 = Some` attach (sentinels are
    /// skipped); attach order is `/dev/vdb`, `/dev/vdc`, …
    pub aux_ro_drives: Vec<AuxRoDrive>,
    /// Directory the erofs payloads live in (`<sha>.erofs`).
    pub bundle_dir: std::path::PathBuf,
}

impl VmConfig {
    pub fn new(
        kernel_path: impl Into<std::path::PathBuf>,
        rootfs_path: impl Into<std::path::PathBuf>,
        memory_mib: u32,
        vcpus: u32,
    ) -> Self {
        Self {
            kernel_path: kernel_path.into(),
            rootfs_path: rootfs_path.into(),
            memory_mib,
            vcpus,
            // Boot args tuned for fast VZ cold-boot. Mirrors what
            // `apple/containerization` uses for its arm64 Linux
            // guests:
            //   - `console=hvc0` — kernel logs to virtio-console 0,
            //     which we wire to host stderr.
            //   - `tsc=reliable` — skip TSC calibration. Apple's
            //     hypervisor exposes a stable timestamp counter; the
            //     calibration loop wastes ~hundreds of ms.
            //   - `panic=0` — kernel panic stops the VM (host
            //     observes via vm-stopped event); no auto-reboot.
            //   - `quiet` — suppresses non-critical kernel logs at
            //     boot. Pair with a kernel built with
            //     CONFIG_CONSOLE_LOGLEVEL_QUIET=4 for full silence.
            //   - `init=/sbin/engram-init` — our minimal init shim
            //     that mounts /proc /sys /dev, exports
            //     ENGRAM_TRANSPORT, and exec's engram-agentd. The
            //     bake injects this at /sbin/engram-init.
            //   - `ip=dhcp` — Linux's IP_PNP path: kernel itself
            //     brings up eth0 and DHCPs for an address against
            //     VZ's NAT before userspace runs. The Kata kernel
            //     ships with CONFIG_IP_PNP_DHCP=y so this is free.
            //     Without it the rootfs would need iproute2 +
            //     dhclient just to get on the network — `node:20-slim`
            //     and the demo bakes carry neither, so the guest
            //     was unreachable. The init shim still has to write
            //     /etc/resolv.conf because IP_PNP doesn't touch the
            //     userspace resolver.
            kernel_cmdline: "console=hvc0 tsc=reliable panic=0 root=/dev/vda rw \
                             quiet init=/sbin/engram-init ip=dhcp"
                .into(),
            aux_ro_drives: Vec::new(),
            bundle_dir: std::path::PathBuf::new(),
        }
    }

    /// ADR 0061: attach these skill bundles (resolved `AuxRoDrive`s) from
    /// `bundle_dir`. Sentinels (`sha256 = None`) are skipped at attach.
    pub fn with_aux_ro_drives(
        mut self,
        drives: Vec<AuxRoDrive>,
        bundle_dir: std::path::PathBuf,
    ) -> Self {
        self.aux_ro_drives = drives;
        self.bundle_dir = bundle_dir;
        self
    }
}

/// Errors from the VZ wrapping layer. Caller maps these onto
/// `engram_core::SandboxError`.
#[derive(Debug)]
pub(crate) enum VzError {
    /// Configuration validation failed (`validateWithError:`). Most
    /// commonly a missing entitlement or invalid memory size; the
    /// inner message is the `NSError.localizedDescription`.
    ConfigInvalid(String),
    /// Disk image attachment failed (e.g. rootfs unreadable).
    AttachmentFailed(String),
    /// `VZVirtualMachine::startWithCompletionHandler` /
    /// stop / pause / resume returned an `NSError`.
    Op(&'static str, String),
    /// Save/restore failed.
    SaveRestore(&'static str, String),
    /// The completion handler's queue was dropped before the handler
    /// fired. Should never happen unless the dispatch infrastructure
    /// is being torn down; surfaced as Internal so it's loud.
    Internal(String),
}

impl std::fmt::Display for VzError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigInvalid(m) => {
                write!(f, "vz config invalid: {m}")
            }
            Self::AttachmentFailed(m) => {
                write!(f, "vz attachment failed: {m}")
            }
            Self::Op(op, m) => write!(f, "vz {op} failed: {m}"),
            Self::SaveRestore(op, m) => write!(f, "vz {op} failed: {m}"),
            Self::Internal(m) => write!(f, "vz internal error: {m}"),
        }
    }
}

impl std::error::Error for VzError {}

impl From<VzError> for engram_core::SandboxError {
    fn from(e: VzError) -> Self {
        match &e {
            VzError::ConfigInvalid(_) | VzError::AttachmentFailed(_) => {
                engram_core::SandboxError::InvalidSpec(e.to_string())
            }
            VzError::SaveRestore(_, _) => engram_core::SandboxError::Snapshot(e.to_string()),
            VzError::Op(_, _) | VzError::Internal(_) => engram_core::SandboxError::Vm(Box::new(e)),
        }
    }
}

/// Send-wrapper to ferry an ObjC reference (Retained<T>, RcBlock,
/// etc.) into a closure dispatched onto the queue. ObjC objects
/// are `!Send` in objc2 0.6, but ARC retain/release is thread-safe
/// and our access pattern serializes through the queue, so the
/// move itself is sound.
///
/// SAFETY: the inner value is dereferenced only inside a closure
/// dispatched onto the per-VM serial `DispatchQueue`. The queue
/// runs that closure on one of its worker threads, never two
/// simultaneously, so there's no concurrent access. Non-mutating
/// retain/release across threads is safe because Apple's ARC
/// counters are atomic.
pub(crate) struct Sendable<T>(pub(crate) T);
unsafe impl<T> Send for Sendable<T> {}
unsafe impl<T> Sync for Sendable<T> {}

impl<T> std::ops::Deref for Sendable<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Clone> Clone for Sendable<T> {
    /// Cloning a `Sendable<Retained<_>>` bumps the ObjC refcount, which
    /// is atomic — sound to do off the dispatch queue. Consumers still
    /// dereference the clone only on the queue.
    fn clone(&self) -> Self {
        Sendable(self.0.clone())
    }
}

/// Owned VZ virtual machine.
///
/// Holds the `Retained<VZVirtualMachine>` plus the per-VM
/// `DispatchQueue`. Public methods are async wrappers that dispatch
/// onto the queue — that's the only safe way to talk to a
/// `VZVirtualMachine`, per Apple's contract.
pub(crate) struct VzVm {
    vm: Retained<VZVirtualMachine>,
    queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: see `SendableVm` for the full argument. The `VzVm` itself
// is treated the same way — the `vm` field is only ever cloned into
// `SendableVm` for dispatch onto the queue; nothing reads or mutates
// it directly across threads.
unsafe impl Send for VzVm {}
unsafe impl Sync for VzVm {}

impl VzVm {
    /// Hand out a Send/Sync clone of the VM pointer for use inside
    /// queue-dispatched closures. The vsock bridge holds onto this
    /// for the lifetime of the VM. Wrapping in `Sendable` is sound as
    /// long as every consumer dereferences only on the dispatch queue
    /// (the bridge does).
    pub(crate) fn raw_clone(&self) -> Sendable<Retained<VZVirtualMachine>> {
        Sendable(self.vm.clone())
    }

    /// Clone the queue handle. The vsock bridge / connector hold onto
    /// it for the lifetime of the VM (dial + cleanup dispatch).
    pub(crate) fn queue_clone(&self) -> DispatchRetained<DispatchQueue> {
        self.queue.clone()
    }

    /// Build a VZ VM from `cfg`, validate the configuration, but
    /// do *not* start it — the caller starts via `start()` once it's
    /// done wiring the vsock bridge (see `vsock_bridge.rs`). VZ vsock
    /// device APIs (`connectToPort`, `setSocketListener:forPort:`)
    /// only operate on a running machine, so the bridge attaches after
    /// `start()`.
    pub fn new(cfg: VmConfig) -> Result<Self, VzError> {
        // Each VM gets its own serial queue. Label is debug-only —
        // shows up in `Activity Monitor` and `lldb`.
        let queue = DispatchQueue::new(&format!("engram-vz-{}", uuid::Uuid::new_v4()), None);

        // Build configuration. None of this needs to be on the
        // queue — the configuration object is plain data; it only
        // becomes "live" when handed to VZVirtualMachine::init.
        let vz_cfg = build_configuration(&cfg)?;

        // Validate. Returns `Result<(), Retained<NSError>>`.
        // SAFETY: vz_cfg is freshly built and live for the call.
        let validation = unsafe { vz_cfg.validateWithError() };
        if let Err(err) = validation {
            return Err(VzError::ConfigInvalid(ns_error_message(&err)));
        }
        // Apple documents save/restore as unsupported for some
        // configurations (USB mass storage, bridged network, etc.).
        // `validateSaveRestoreSupportWithError` says yes/no without
        // having to actually attempt a restore.
        let save_restore_check = unsafe { vz_cfg.validateSaveRestoreSupportWithError() };
        if let Err(err) = save_restore_check {
            tracing::warn!(
                error = %ns_error_message(&err),
                "VM config does NOT support save/restore — snapshot/restore will fail"
            );
        } else {
            tracing::debug!("VM config validated for save/restore support");
        }

        // Construct the VM. Per Apple, init returns `instancetype`
        // and never `nil` for the queue-aware initializer.
        // SAFETY: cfg validated above; queue lifetime is tied to
        // the returned VzVm.
        let vm = unsafe {
            VZVirtualMachine::initWithConfiguration_queue(
                VZVirtualMachine::alloc(),
                &vz_cfg,
                &queue,
            )
        };

        Ok(Self { vm, queue })
    }

    /// Start the VM. Resolves once VZ's `startWithCompletionHandler`
    /// fires its callback (success) or returns an NSError (failure).
    pub async fn start(&self) -> Result<(), VzError> {
        self.dispatch_op("start", |vm, completion| {
            // SAFETY: vm is live for the duration of the dispatch
            // call; completion is a heap-allocated RcBlock owned by
            // the queue's worker until invoked.
            unsafe { vm.startWithCompletionHandler(completion) }
        })
        .await
    }

    /// Request a clean stop. Resolves on completion-handler fire.
    pub async fn stop(&self) -> Result<(), VzError> {
        self.dispatch_op("stop", |vm, completion| {
            // SAFETY: see `start`.
            unsafe { vm.stopWithCompletionHandler(completion) }
        })
        .await
    }

    /// Pause the VM. Required before `save`. Used by snapshot
    /// in task 29 — reachable but unused as of task 27.
    #[allow(dead_code)]
    pub async fn pause(&self) -> Result<(), VzError> {
        self.dispatch_op("pause", |vm, completion| {
            // SAFETY: see `start`.
            unsafe { vm.pauseWithCompletionHandler(completion) }
        })
        .await
    }

    /// Resume from a paused state. Used by snapshot in task 29.
    #[allow(dead_code)]
    pub async fn resume(&self) -> Result<(), VzError> {
        self.dispatch_op("resume", |vm, completion| {
            // SAFETY: see `start`.
            unsafe { vm.resumeWithCompletionHandler(completion) }
        })
        .await
    }

    /// Save the paused VM's full state (memory + device state) to
    /// `dest`. VM must be in `.paused`. Used by snapshot in task 29.
    #[allow(dead_code)]
    pub async fn save(&self, dest: &Path) -> Result<(), VzError> {
        let url = Sendable(nsurl_for_path(dest));
        self.dispatch_op_save_restore("save", move |vm, completion| {
            // SAFETY: see `start`. url lives until the handler fires
            // because it's captured by the FnOnce that we passed to
            // dispatch_op_save_restore.
            unsafe { vm.saveMachineStateToURL_completionHandler(&url, completion) }
        })
        .await
    }

    /// Restore from a save file. Must be called on a freshly-built
    /// VM with a configuration that matches the source. Used by
    /// snapshot in task 29.
    #[allow(dead_code)]
    pub async fn restore(&self, src: &Path) -> Result<(), VzError> {
        let url = Sendable(nsurl_for_path(src));
        self.dispatch_op_save_restore("restore", move |vm, completion| {
            // SAFETY: see `start`.
            unsafe { vm.restoreMachineStateFromURL_completionHandler(&url, completion) }
        })
        .await
    }

    /// Internal: dispatch a VZ async op (start/stop/pause/resume)
    /// onto the queue and bridge its NSError-returning completion
    /// handler back to a tokio oneshot.
    async fn dispatch_op<F>(&self, op: &'static str, schedule: F) -> Result<(), VzError>
    where
        F: FnOnce(&VZVirtualMachine, &block2::DynBlock<dyn Fn(*mut NSError)>) + Send + 'static,
    {
        self.run_completion("op", op, schedule).await
    }

    /// Same shape as `dispatch_op`, but tags the error variant as
    /// `SaveRestore` so the caller can distinguish snapshot failures
    /// from lifecycle failures. Used by snapshot in task 29.
    #[allow(dead_code)]
    async fn dispatch_op_save_restore<F>(
        &self,
        op: &'static str,
        schedule: F,
    ) -> Result<(), VzError>
    where
        F: FnOnce(&VZVirtualMachine, &block2::DynBlock<dyn Fn(*mut NSError)>) + Send + 'static,
    {
        self.run_completion("save_restore", op, schedule).await
    }

    /// Internal: dispatch a VZ async op onto the queue and bridge
    /// its NSError-returning completion handler to a tokio oneshot.
    ///
    /// `kind` controls the error variant ("op" → `VzError::Op`,
    /// "save_restore" → `VzError::SaveRestore`). Both produce the
    /// same wire shape; this just carves error categorization for
    /// the caller.
    async fn run_completion<F>(
        &self,
        kind: &'static str,
        op: &'static str,
        schedule: F,
    ) -> Result<(), VzError>
    where
        F: FnOnce(&VZVirtualMachine, &block2::DynBlock<dyn Fn(*mut NSError)>) + Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<Result<(), VzError>>();
        let vm = Sendable(self.vm.clone());
        let tx = Mutex::new(Some(tx));
        let block = RcBlock::new(move |err: *mut NSError| {
            let result: Result<(), VzError> = if err.is_null() {
                Ok(())
            } else {
                // SAFETY: VZ guarantees `err` is a valid retained
                // NSError pointer when non-null. We re-retain to
                // bump the refcount before reading the message.
                let err = unsafe { Retained::retain(err) };
                let msg = err
                    .map(|e| ns_error_message(&e))
                    .unwrap_or_else(|| "<nil error>".into());
                Err(match kind {
                    "save_restore" => VzError::SaveRestore(op, msg),
                    _ => VzError::Op(op, msg),
                })
            };
            if let Some(tx) = tx.lock().expect("oneshot mutex poisoned").take() {
                let _ = tx.send(result);
            }
        });
        let block = Sendable(block);
        self.queue.exec_async(move || {
            schedule(&vm, &block);
        });
        rx.await
            .map_err(|_| VzError::Internal(format!("{op}: completion channel dropped")))?
    }
}

/// ADR 0061: host path of a resolved skill generation for the VZ backend.
/// Content-keyed `<bundle_dir>/<sha>.erofs` — VZ stages erofs where FC
/// stages squashfs (the Kata VZ kernel has no CONFIG_SQUASHFS). The sha
/// comes from the host's `current.json` stamp via the coordinator's
/// resolved `AuxRoDrive.sha256`, so path and content never disagree.
pub(crate) fn staged_erofs_path(bundle_dir: &std::path::Path, sha: &str) -> std::path::PathBuf {
    bundle_dir.join(format!("{sha}.erofs"))
}

/// ADR 0062: order aux RO drives by ascending reserved slot for attach.
///
/// The guest init shim mounts each attached bundle device at a *sequential*
/// `/opt/engram/dyn/<i>` in `/dev/vd*` enumeration order (i.e. attach order) —
/// it reads no slot metadata. On FC that still yields `dyn/<i> == slot i`
/// because the base snapshot attaches all `RESERVED_SLOTS` as sentinels in slot
/// order and `patch_drive` swaps by `drive_id`. VZ has no sentinel pool: it
/// attaches only the *resolved* drives, so attach order alone decides the guest
/// index. The coordinator pushes the harness (slot 0) LAST in `selected_mounts`
/// (after the skills, slots 1..), so a naive slice-order attach lands the
/// harness at `dyn/<n_skills>` while the coordinator `exec`s the FIXED
/// `/opt/engram/dyn/0/harness` → `spawn ... No such file or directory`.
/// Sorting by slot restores slot order, so slot 0 (harness) is always attached
/// first and mounts at `dyn/0`. Skills are `mount.json`-discovered and thus
/// position-independent, but staying in slot order keeps them deterministic too.
/// Drives without a parseable slot sort last (defensive; every real aux drive
/// carries a `dyn_<i>` id). Stable sort preserves relative order within a slot.
pub(crate) fn aux_drives_in_slot_order(drives: &[AuxRoDrive]) -> Vec<&AuxRoDrive> {
    let mut ordered: Vec<&AuxRoDrive> = drives.iter().collect();
    ordered.sort_by_key(|d| d.slot_index().unwrap_or(usize::MAX));
    ordered
}

/// Build a fully-configured `VZVirtualMachineConfiguration` for
/// `cfg`. Linux boot, virtio-block rootfs, a `VZVirtioSocketDevice`
/// (real virtio-vsock) for the host↔guest control/harness/relay
/// channels, virtio-net NAT, and a separate single-port virtio-console
/// wired to host stderr for the kernel boot log.
///
/// ADR 0066 Phase 2: the transport swapped from a multi-port
/// virtio-console (single byte stream per port → head-of-line
/// blocking when a persistent connection monopolises a port) back to
/// virtio-vsock, which muxes any number of concurrent streams per
/// port. The Kata guest kernel VZ boots (`just pull-kernel`) ships
/// `CONFIG_VIRTIO_VSOCKETS=y` built-in, so the earlier "console is
/// universally compiled in, vsock isn't" constraint no longer applies.
/// The `vsock_bridge` attaches per-port listeners + dials after
/// `VZVirtualMachine::start`.
fn build_configuration(cfg: &VmConfig) -> Result<Retained<VZVirtualMachineConfiguration>, VzError> {
    // SAFETY: every call below operates on freshly-allocated
    // ObjC objects whose lifetimes are managed via Retained. None
    // of them have started running on a queue yet, so there's no
    // concurrent access to worry about.
    unsafe {
        let vz_cfg = VZVirtualMachineConfiguration::new();

        // Boot loader: Linux kernel + commandline.
        let kernel_url = nsurl_for_path(&cfg.kernel_path);
        let bootloader =
            VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &kernel_url);
        let cmdline = NSString::from_str(&cfg.kernel_cmdline);
        bootloader.setCommandLine(&cmdline);
        // Upcast to VZBootLoader before storing on the cfg, since
        // setBootLoader takes a VZBootLoader.
        let bootloader_super: Retained<VZBootLoader> = Retained::cast_unchecked(bootloader);
        vz_cfg.setBootLoader(Some(&bootloader_super));

        // CPU + memory.
        vz_cfg.setCPUCount(cfg.vcpus as objc2_foundation::NSUInteger);
        vz_cfg.setMemorySize((cfg.memory_mib as u64) * 1024 * 1024);

        // Storage devices, in attach order so `/dev/vda` is the
        // rootfs and (when present) `/dev/vdb` is the harness
        // substrate:
        //   - rootfs.ext4 — read-write, so the guest can mutate
        //     /workspace state during the session.
        //   - harness-substrate.img — read-only ext4 image of the
        //     host's `cfg.harnesses_dir`. Init shim mounts it at
        //     `/run/engram/harnesses`. Same wire as the FC backend.
        let mut storage: Vec<Retained<VZStorageDeviceConfiguration>> = Vec::with_capacity(2);

        let rootfs_url = nsurl_for_path(&cfg.rootfs_path);
        let attachment = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &rootfs_url,
            false,
        )
        .map_err(|err| VzError::AttachmentFailed(ns_error_message(&err)))?;
        let attachment_super: Retained<objc2_virtualization::VZStorageDeviceAttachment> =
            Retained::cast_unchecked(attachment);
        let block_dev = VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &attachment_super,
        );
        storage.push(Retained::cast_unchecked(block_dev));

        // ADR 0061/0062: attach each resolved bundle as a read-only erofs
        // virtio-blk image. Order by ascending reserved slot (NOT the
        // coordinator's slice order, which pushes the harness last) so the
        // harness (slot 0) is attached first and the guest init shim mounts it
        // at /opt/engram/dyn/0 — the FIXED path the coordinator `exec`s. The
        // guest indexes dyn/<i> by attach order alone, so slice order would
        // strand the harness at dyn/<n_skills>. See `aux_drives_in_slot_order`.
        // The guest RO-mounts each device at /opt/engram/dyn/<i>; agentd reads
        // its mount.json to wire skills (harness is `exec`'d, not wired).
        // Sentinel slots (sha = None) are skipped, so base-snapshot capture
        // (whose spec carries only `reserved_slot` placeholders) attaches
        // nothing and the base snapshot stays skill-agnostic.
        for drive in aux_drives_in_slot_order(&cfg.aux_ro_drives) {
            let Some(sha) = drive.sha256.as_deref() else {
                continue;
            };
            let path = staged_erofs_path(&cfg.bundle_dir, sha);
            if !path.exists() {
                return Err(VzError::AttachmentFailed(format!(
                    "skill bundle {} not staged at {} — run `just bundles-vz`",
                    drive.drive_id,
                    path.display()
                )));
            }
            tracing::info!(
                drive_id = %drive.drive_id,
                path = %path.display(),
                "vz: attaching aux erofs drive"
            );
            let url = nsurl_for_path(&path);
            let att = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &url,
                true, // read-only
            )
            .map_err(|err| VzError::AttachmentFailed(ns_error_message(&err)))?;
            let att_super: Retained<objc2_virtualization::VZStorageDeviceAttachment> =
                Retained::cast_unchecked(att);
            let dev = VZVirtioBlockDeviceConfiguration::initWithAttachment(
                VZVirtioBlockDeviceConfiguration::alloc(),
                &att_super,
            );
            storage.push(Retained::cast_unchecked(dev));
        }

        let storage_array: Retained<NSArray<VZStorageDeviceConfiguration>> =
            NSArray::from_retained_slice(&storage);
        vz_cfg.setStorageDevices(&storage_array);

        // Network: NAT (the macOS host shares its network with the
        // guest, no per-sandbox iptables filtering). FC doesn't
        // have allow_hosts filtering today either; both backends
        // defer it.
        let nat = VZNATNetworkDeviceAttachment::new();
        let net_dev = VZVirtioNetworkDeviceConfiguration::new();
        let nat_super: Retained<objc2_virtualization::VZNetworkDeviceAttachment> =
            Retained::cast_unchecked(nat);
        net_dev.setAttachment(Some(&nat_super));
        let net_dev_super: Retained<objc2_virtualization::VZNetworkDeviceConfiguration> =
            Retained::cast_unchecked(net_dev);
        let network_array: Retained<NSArray<objc2_virtualization::VZNetworkDeviceConfiguration>> =
            NSArray::from_retained_slice(&[net_dev_super]);
        vz_cfg.setNetworkDevices(&network_array);

        // virtio-fs is no longer used — the harness substrate is now
        // attached as a read-only virtio-blk image (task 2 of the FC
        // parity work), and `WorkspaceSpec::LocalMount` is gone.
        // The directory-sharing device path stays unset.

        // Real virtio-vsock — a single device, shared across all ports.
        // The `vsock_bridge` (vsock_bridge.rs) attaches per-port
        // listeners (harness 1026 / ready 1027 / upload 1029) and dials
        // host→guest ports (agentd 1024 / relay 1030) after
        // `VZVirtualMachine::initWithConfiguration_queue`. Unlike the
        // retired multi-port virtio-console, vsock muxes any number of
        // concurrent streams per port, so a persistent forwarded
        // connection (an HMR WebSocket, a noVNC stream) can't head-of-line
        // block other forwarded connections (ADR 0066 Phase 2).
        let vsock_dev = VZVirtioSocketDeviceConfiguration::new();
        let vsock_dev_super: Retained<VZSocketDeviceConfiguration> =
            Retained::cast_unchecked(vsock_dev);
        let socket_array: Retained<NSArray<VZSocketDeviceConfiguration>> =
            NSArray::from_retained_slice(&[vsock_dev_super]);
        vz_cfg.setSocketDevices(&socket_array);

        // Serial port: virtio-console wired to host stderr so kernel
        // boot logs + the engram-init shim's output land in the
        // coord's tracing stream. Critical for debugging "why
        // didn't bootstrap come up" — without this, a kernel panic
        // or init failure is invisible from the host.
        //
        // Opt-out: ENGRAM_VZ_SILENCE_CONSOLE=1 wires /dev/null
        // instead, for production-style runs that don't want guest
        // chatter mixed into coord logs.
        let attach_console = std::env::var("ENGRAM_VZ_SILENCE_CONSOLE")
            .map(|v| v == "0" || v.is_empty())
            .unwrap_or(true);
        let serial_cfg = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        if attach_console {
            // STDERR_FILENO = 2. NSFileHandle::fileHandleWithStandardError
            // returns a singleton that wraps the process's fd 2.
            let stderr_handle = NSFileHandle::fileHandleWithStandardError();
            let attachment =
                VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                    VZFileHandleSerialPortAttachment::alloc(),
                    None,
                    Some(&stderr_handle),
                );
            let attachment_super: Retained<VZSerialPortAttachment> =
                Retained::cast_unchecked(attachment);
            serial_cfg.setAttachment(Some(&attachment_super));
        }
        let serial_super: Retained<VZSerialPortConfiguration> =
            Retained::cast_unchecked(serial_cfg);
        let serial_array: Retained<NSArray<VZSerialPortConfiguration>> =
            NSArray::from_retained_slice(&[serial_super]);
        vz_cfg.setSerialPorts(&serial_array);

        Ok(vz_cfg)
    }
}

/// Build an `NSURL` from a host filesystem path.
fn nsurl_for_path(path: &Path) -> Retained<NSURL> {
    let path_string = NSString::from_str(&path.to_string_lossy());
    NSURL::fileURLWithPath(&path_string)
}

/// Extract a UTF-8 message from an NSError, including the
/// numeric code + domain + a few well-known userInfo strings so
/// opaque errors (e.g. `restoreMachineStateFromURL` returning the
/// generic VZErrorRestore=12) can be matched against
/// `VZErrorCode` constants and Apple's specific failure reasons.
fn ns_error_message(err: &NSError) -> String {
    use objc2::rc::Retained;
    use objc2_foundation::NSString;

    let desc = err.localizedDescription().to_string();
    let code = err.code();
    let domain = err.domain().to_string();
    let failure_reason = err
        .localizedFailureReason()
        .map(|s| s.to_string())
        .unwrap_or_default();
    let recovery = err
        .localizedRecoverySuggestion()
        .map(|s| s.to_string())
        .unwrap_or_default();
    // Apple frequently nests a more-specific NSError under
    // `NSUnderlyingError`. Pull it if present — that's where the
    // real "you asked for X but Y" is.
    let underlying = {
        let user_info = err.userInfo();
        let key = NSString::from_str("NSUnderlyingError");
        // SAFETY: objectForKey returns Option<Retained<AnyObject>>;
        // we narrow to NSError if the runtime class matches.
        let raw: Option<Retained<objc2::runtime::AnyObject>> = user_info.objectForKey(&key);
        raw.and_then(|obj| obj.downcast::<NSError>().ok())
            .map(|nested| {
                let n_desc = nested.localizedDescription().to_string();
                let n_code = nested.code();
                let n_domain = nested.domain().to_string();
                format!("{n_desc} [domain={n_domain} code={n_code}]")
            })
            .unwrap_or_default()
    };
    let mut out = format!("{desc} [domain={domain} code={code}]");
    if !failure_reason.is_empty() {
        out.push_str(&format!(" reason={failure_reason}"));
    }
    if !recovery.is_empty() {
        out.push_str(&format!(" recovery={recovery}"));
    }
    if !underlying.is_empty() {
        out.push_str(&format!(" underlying={underlying}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without the `com.apple.security.virtualization` entitlement,
    /// `validateWithError` returns NSError code 7 (or a similar
    /// "configuration invalid" code). This test confirms that path
    /// surfaces as `VzError::ConfigInvalid` with a useful message,
    /// which is the most common dev-mode failure (forgot
    /// `just vz-codesign`).
    ///
    /// Because the test binary itself is unsigned, we expect this
    /// to fail with a config-invalid error, not pass — so a green
    /// run = the plumbing works end-to-end *until* the entitlement
    /// check.
    #[test]
    #[ignore = "requires a kernel + rootfs FILE present on disk (any bytes — \
                CI touches an empty /tmp/engram-vz-rootfs.ext4); run with \
                --ignored after `just pull-kernel`."]
    fn config_validation_surfaces_clear_error_without_entitlement() {
        let kernel =
            std::path::PathBuf::from(std::env::var("ENGRAM_VZ_KERNEL_PATH").unwrap_or_else(|_| {
                std::env::var("HOME").unwrap_or_default() + "/.cache/engram-vz-test/vmlinux-arm64"
            }));
        let rootfs = std::path::PathBuf::from("/tmp/engram-vz-rootfs.ext4");
        if !kernel.exists() || !rootfs.exists() {
            eprintln!(
                "skipping; need kernel at {} and rootfs at {}",
                kernel.display(),
                rootfs.display()
            );
            return;
        }
        let cfg = VmConfig::new(kernel, rootfs, 512, 1);
        // Without entitlement we expect ConfigInvalid; with it,
        // VzVm::new should succeed.
        match VzVm::new(cfg) {
            Ok(_vm) => eprintln!("VM constructed (entitlement is present)"),
            Err(VzError::ConfigInvalid(msg)) => {
                eprintln!("got expected ConfigInvalid: {msg}");
                assert!(!msg.is_empty(), "error message must not be empty");
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn staged_erofs_path_is_content_keyed() {
        let p = super::staged_erofs_path(std::path::Path::new("/var/shared"), "abc123");
        assert_eq!(p, std::path::PathBuf::from("/var/shared/abc123.erofs"));
    }

    /// ADR 0062 regression: the coordinator builds `selected_mounts` as
    /// `[skill@dyn_1, skill@dyn_2, harness@dyn_0]` (harness pushed LAST). VZ's
    /// attach order fixes the guest `dyn/<i>` index, so it must attach
    /// slot-ascending — otherwise the harness lands at `dyn/2` and the
    /// coordinator's `exec /opt/engram/dyn/0/harness` hits ENOENT. This pins the
    /// harness (slot 0) to the front regardless of input order.
    #[test]
    fn aux_drives_attach_in_slot_order_harness_first() {
        let skill1 = AuxRoDrive {
            sha256: Some("s1".into()),
            ..AuxRoDrive::reserved_slot(1)
        };
        let skill2 = AuxRoDrive {
            sha256: Some("s2".into()),
            ..AuxRoDrive::reserved_slot(2)
        };
        let harness = AuxRoDrive {
            sha256: Some("hh".into()),
            ..AuxRoDrive::reserved_slot(0)
        };
        // Coordinator order: skills first, harness last.
        let input = vec![skill1, skill2, harness];
        let ordered = super::aux_drives_in_slot_order(&input);
        let slots: Vec<_> = ordered.iter().map(|d| d.slot_index()).collect();
        assert_eq!(slots, vec![Some(0), Some(1), Some(2)], "slot-ascending");
        assert_eq!(
            ordered[0].sha256.as_deref(),
            Some("hh"),
            "harness attaches first → guest dyn/0"
        );
    }

    #[test]
    fn vm_config_default_cmdline_routes_console_and_root() {
        let cfg = VmConfig::new("/k", "/r", 512, 1);
        assert!(cfg.kernel_cmdline.contains("console=hvc0"));
        assert!(cfg.kernel_cmdline.contains("root=/dev/vda"));
    }

    /// Smoke test for the full ObjC plumbing in `VzVm::new` —
    /// allocates VZVirtualMachineConfiguration, attaches a kernel
    /// boot loader pointing at a tempfile, attaches a virtio-block
    /// pointing at a tempfile rootfs, attaches vsock + net + serial
    /// devices, calls `validateWithError:`. On an unsigned test
    /// binary (no `com.apple.security.virtualization` entitlement)
    /// this returns a configuration-invalid NSError, which we
    /// surface as `VzError::ConfigInvalid`. A successful return
    /// means the test binary *was* signed (e.g. via
    /// `just vz-codesign`); both outcomes are acceptable, but
    /// neither should panic — that would mean the ObjC bindings
    /// are wired wrong.
    #[test]
    fn vz_vm_new_full_plumbing_runs_or_fails_cleanly() {
        let kernel = tempfile::NamedTempFile::new().unwrap();
        let rootfs = tempfile::NamedTempFile::new().unwrap();
        // Apple's lower-bound on memory is enforced by VZ; 512 MiB
        // is well above any plausible minimum.
        let cfg = VmConfig::new(kernel.path(), rootfs.path(), 512, 1);
        match VzVm::new(cfg) {
            Ok(_vm) => {
                eprintln!("vz_vm_new succeeded — test binary carries entitlement");
            }
            Err(VzError::ConfigInvalid(msg)) => {
                eprintln!("vz_vm_new failed with ConfigInvalid: {msg}");
                assert!(
                    !msg.is_empty(),
                    "ConfigInvalid must carry a message from NSError"
                );
            }
            Err(VzError::AttachmentFailed(msg)) => {
                eprintln!("vz_vm_new failed with AttachmentFailed: {msg}");
                assert!(!msg.is_empty(), "AttachmentFailed must carry a message");
            }
            Err(other) => panic!("unexpected error from VzVm::new: {other}"),
        }
    }
}
