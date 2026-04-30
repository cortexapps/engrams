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
use objc2_foundation::{NSArray, NSError, NSString, NSURL};
use objc2_foundation::NSFileHandle;
use objc2_virtualization::{
    VZBootLoader, VZDiskImageStorageDeviceAttachment, VZFileHandleSerialPortAttachment,
    VZLinuxBootLoader, VZNATNetworkDeviceAttachment, VZSerialPortAttachment,
    VZSerialPortConfiguration, VZStorageDeviceConfiguration,
    VZVirtioBlockDeviceConfiguration, VZVirtioConsoleDeviceSerialPortConfiguration,
    VZVirtioNetworkDeviceConfiguration, VZVirtualMachine, VZVirtualMachineConfiguration,
};

use crate::console_bridge::{build_console_device, ConsolePortFds};
use tokio::sync::oneshot;

/// Per-VM declarative inputs. Built once at create time.
#[derive(Clone, Debug)]
pub(crate) struct VmConfig {
    pub kernel_path: std::path::PathBuf,
    pub rootfs_path: std::path::PathBuf,
    pub memory_mib: u32,
    pub vcpus: u32,
    /// Linux kernel command line. Default points root at /dev/vda
    /// (the first virtio-block device, which is the only one we
    /// attach) and routes the console to hvc0.
    pub kernel_cmdline: String,
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
            // `init=/sbin/engram-init` mirrors the FC backend's
            // default — the engram-baked rootfs ships an init shim
            // at /sbin/engram-init that exec's engram-agentd on
            // vsock and spawns engram-bootstrap as a supervisor.
            // Without this, the guest boots /sbin/init (systemd
            // or none) and nothing listens on vsock 1024/1025.
            kernel_cmdline:
                "console=hvc0 root=/dev/vda rw quiet init=/sbin/engram-init".into(),
        }
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
            VzError::Op(_, _) | VzError::Internal(_) => {
                engram_core::SandboxError::Vm(Box::new(e))
            }
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
    /// Build a VZ VM from `cfg`, validate the configuration, but
    /// do *not* start it — the caller starts via `start()` once it's
    /// done wiring the console UDS bridge (see `console_bridge.rs`).
    ///
    /// Returns the VM alongside the host-side `ConsolePortFds` that
    /// the `ConsoleBridge` consumes after `start()`. Splitting this
    /// out keeps the device-config (which must be set before init)
    /// and the bridge wiring (which runs after start) on opposite
    /// sides of `VZVirtualMachine::initWithConfiguration_queue`.
    pub fn new(cfg: VmConfig) -> Result<(Self, ConsolePortFds), VzError> {
        // Each VM gets its own serial queue. Label is debug-only —
        // shows up in `Activity Monitor` and `lldb`.
        let queue =
            DispatchQueue::new(&format!("engram-vz-{}", uuid::Uuid::new_v4()), None);

        // Build configuration. None of this needs to be on the
        // queue — the configuration object is plain data; it only
        // becomes "live" when handed to VZVirtualMachine::init.
        let (vz_cfg, port_fds) = build_configuration(&cfg)?;

        // Validate. Returns `Result<(), Retained<NSError>>`.
        // SAFETY: vz_cfg is freshly built and live for the call.
        let validation = unsafe { vz_cfg.validateWithError() };
        if let Err(err) = validation {
            return Err(VzError::ConfigInvalid(ns_error_message(&err)));
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

        Ok((Self { vm, queue }, port_fds))
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
        F: FnOnce(&VZVirtualMachine, &block2::DynBlock<dyn Fn(*mut NSError)>)
            + Send
            + 'static,
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
        F: FnOnce(&VZVirtualMachine, &block2::DynBlock<dyn Fn(*mut NSError)>)
            + Send
            + 'static,
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
        F: FnOnce(&VZVirtualMachine, &block2::DynBlock<dyn Fn(*mut NSError)>)
            + Send
            + 'static,
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

/// Build a fully-configured `VZVirtualMachineConfiguration` for
/// `cfg`. Linux boot, virtio-block rootfs, multi-port virtio-console
/// for the host↔guest control channels, virtio-net NAT, and a
/// separate single-port virtio-console wired to host stderr for the
/// kernel boot log.
///
/// Returns the configuration alongside the host-side
/// `ConsolePortFds` that `ConsoleBridge::start` consumes after
/// `VZVirtualMachine::start`.
fn build_configuration(
    cfg: &VmConfig,
) -> Result<(Retained<VZVirtualMachineConfiguration>, ConsolePortFds), VzError> {
    // SAFETY: every call below operates on freshly-allocated
    // ObjC objects whose lifetimes are managed via Retained. None
    // of them have started running on a queue yet, so there's no
    // concurrent access to worry about.
    unsafe {
        let vz_cfg = VZVirtualMachineConfiguration::new();

        // Boot loader: Linux kernel + commandline.
        let kernel_url = nsurl_for_path(&cfg.kernel_path);
        let bootloader = VZLinuxBootLoader::initWithKernelURL(
            VZLinuxBootLoader::alloc(),
            &kernel_url,
        );
        let cmdline = NSString::from_str(&cfg.kernel_cmdline);
        bootloader.setCommandLine(&cmdline);
        // Upcast to VZBootLoader before storing on the cfg, since
        // setBootLoader takes a VZBootLoader.
        let bootloader_super: Retained<VZBootLoader> =
            Retained::cast_unchecked(bootloader);
        vz_cfg.setBootLoader(Some(&bootloader_super));

        // CPU + memory.
        vz_cfg.setCPUCount(cfg.vcpus as objc2_foundation::NSUInteger);
        vz_cfg.setMemorySize((cfg.memory_mib as u64) * 1024 * 1024);

        // Storage: virtio-block on the rootfs.ext4. Read-write so
        // the guest can mutate /workspace state during the session.
        let rootfs_url = nsurl_for_path(&cfg.rootfs_path);
        let attachment = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &rootfs_url,
            false,
        )
        .map_err(|err| VzError::AttachmentFailed(ns_error_message(&err)))?;
        let attachment_super: Retained<
            objc2_virtualization::VZStorageDeviceAttachment,
        > = Retained::cast_unchecked(attachment);
        let block_dev = VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &attachment_super,
        );
        let block_dev_super: Retained<VZStorageDeviceConfiguration> =
            Retained::cast_unchecked(block_dev);
        let storage_array: Retained<NSArray<VZStorageDeviceConfiguration>> =
            NSArray::from_retained_slice(&[block_dev_super]);
        vz_cfg.setStorageDevices(&storage_array);

        // Network: NAT (the macOS host shares its network with the
        // guest, no per-sandbox iptables filtering). FC doesn't
        // have allow_hosts filtering today either; both backends
        // defer it.
        let nat = VZNATNetworkDeviceAttachment::new();
        let net_dev = VZVirtioNetworkDeviceConfiguration::new();
        let nat_super: Retained<
            objc2_virtualization::VZNetworkDeviceAttachment,
        > = Retained::cast_unchecked(nat);
        net_dev.setAttachment(Some(&nat_super));
        let net_dev_super: Retained<
            objc2_virtualization::VZNetworkDeviceConfiguration,
        > = Retained::cast_unchecked(net_dev);
        let network_array: Retained<
            NSArray<objc2_virtualization::VZNetworkDeviceConfiguration>,
        > = NSArray::from_retained_slice(&[net_dev_super]);
        vz_cfg.setNetworkDevices(&network_array);

        // Multi-port virtio-console for the host↔guest control
        // channels (agentd, bootstrap, harness). Replaces the
        // earlier virtio-vsock device since virtio-console is
        // universally compiled into Linux kernels — no
        // CONFIG_VIRTIO_VSOCKETS=y requirement on the rootfs's
        // kernel. The bridge consumes the returned fds after
        // VM start (see console_bridge.rs).
        let (console_dev, port_fds) =
            build_console_device().map_err(|e| VzError::ConfigInvalid(e.to_string()))?;
        let console_dev_super: Retained<
            objc2_virtualization::VZConsoleDeviceConfiguration,
        > = Retained::cast_unchecked(console_dev);
        let console_array: Retained<
            NSArray<objc2_virtualization::VZConsoleDeviceConfiguration>,
        > = NSArray::from_retained_slice(&[console_dev_super]);
        vz_cfg.setConsoleDevices(&console_array);

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
            let attachment = VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
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

        Ok((vz_cfg, port_fds))
    }
}

/// Build an `NSURL` from a host filesystem path.
fn nsurl_for_path(path: &Path) -> Retained<NSURL> {
    let path_string = NSString::from_str(&path.to_string_lossy());
    NSURL::fileURLWithPath(&path_string)
}

/// Extract a UTF-8 message from an NSError.
fn ns_error_message(err: &NSError) -> String {
    err.localizedDescription().to_string()
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
    #[ignore = "requires a kernel + rootfs file present on disk; \
                ignored by default. Run with --ignored on a host that \
                has just vz-bake-kernel + just vz-bake-claude artifacts."]
    fn config_validation_surfaces_clear_error_without_entitlement() {
        let kernel = std::path::PathBuf::from(
            std::env::var("ENGRAM_VZ_KERNEL_PATH").unwrap_or_else(|_| {
                std::env::var("HOME").unwrap_or_default()
                    + "/.cache/engram-vz-test/vmlinuz-arm64"
            }),
        );
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
            Ok((_vm, _fds)) => eprintln!("VM constructed (entitlement is present)"),
            Err(VzError::ConfigInvalid(msg)) => {
                eprintln!("got expected ConfigInvalid: {msg}");
                assert!(!msg.is_empty(), "error message must not be empty");
            }
            Err(other) => panic!("unexpected error: {other}"),
        }
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
            Ok((_vm, _fds)) => {
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
