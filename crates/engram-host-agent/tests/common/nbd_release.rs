//! Make sure that a test releases the `/dev/nbdN` devices it bound before
//! its process exits.
//!
//! The CI NBD lane runs its test binaries one after the other on the same
//! `/dev/nbd*` devices. Dropping an `NbdHandle` sends the netlink DISCONNECT
//! from a detached thread. When the test process exits before that thread
//! runs, the kernel keeps the device bound to a dead pid with no server, and
//! the next test binary that uses the device waits in
//! `NbdSlotAllocator::acquire` until nextest stops it at 180 s. In CI, a
//! 16-sector binding from
//! `forked_attach_first_flush_mints_private_id_and_leaves_base_chain_untouched`
//! made `high_offset_write_reads_back_through_the_device` time out this way.
//!
//! [`NbdReleaseGuard`] moves the failure to the test that bound the device.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use engram_host_agent::disk_daemon::nbd_netlink::disconnect_device;

/// The kernel normally releases a device in milliseconds. A device that is
/// still bound after this time is a leak.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);

/// True when the kernel holds no binding on `device`: no sysfs `pid` file
/// and a sysfs `size` of `0`. An unreadable `size` is "not released".
///
/// This is `device_is_free` in `src/disk_daemon/slot.rs` without its
/// open-holder (`O_EXCL`) check. A holder is an open file descriptor, and
/// the kernel closes all descriptors of a process when the process exits,
/// so a holder in this test cannot stop the next test binary. Only the
/// kernel binding stays after the exit. Also, an `O_EXCL` open from this
/// probe is itself an opener that can make a CONNECT fail with `EBUSY`.
pub fn kernel_released(device: &Path) -> bool {
    let Some(name) = device.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if Path::new(&format!("/sys/block/{name}/pid")).exists() {
        return false;
    }
    std::fs::read_to_string(format!("/sys/block/{name}/size"))
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

/// The sysfs `pid` and `size` of `device`, for a failure message.
fn sysfs_state(device: &Path) -> (Option<String>, Option<String>) {
    let name = device.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let read = |attr: &str| {
        std::fs::read_to_string(format!("/sys/block/{name}/{attr}"))
            .ok()
            .map(|s| s.trim().to_string())
    };
    (read("pid"), read("size"))
}

/// The kernel index of `/dev/nbdN` (`N`).
fn nbd_index(device: &Path) -> Option<u32> {
    device
        .file_name()?
        .to_str()?
        .strip_prefix("nbd")?
        .parse()
        .ok()
}

/// When dropped, waits until the kernel has released each device.
///
/// Make the guard as soon as the device paths are known, BEFORE the NBD
/// state, pool, or backend variables. Rust drops locals in the opposite
/// order of their declaration, so the guard then drops after the values
/// whose drop starts the release. The guard also runs when the test
/// panics, which an end-of-test call does not do.
///
/// - Normal drop: a device that is still bound after 10 s fails the test
///   with its sysfs pid and size.
/// - Drop during a panic: the test has already failed, and its teardown
///   did not run. The guard sends a DISCONNECT to each device (a device
///   that a test abandoned on purpose has no other release), waits for
///   the release, and writes a message on a timeout. It does not panic,
///   because a second panic aborts the process.
///
/// A test that leaves a device bound on purpose must disconnect it before
/// the guard drops.
pub struct NbdReleaseGuard {
    devices: Vec<PathBuf>,
}

impl NbdReleaseGuard {
    pub fn new<P: AsRef<Path>>(devices: impl IntoIterator<Item = P>) -> Self {
        Self {
            devices: devices
                .into_iter()
                .map(|d| d.as_ref().to_path_buf())
                .collect(),
        }
    }
}

impl Drop for NbdReleaseGuard {
    fn drop(&mut self) {
        let panicking = std::thread::panicking();
        if panicking {
            for device in &self.devices {
                if let Some(index) = nbd_index(device) {
                    let _ = disconnect_device(index);
                }
            }
        }
        // Drop cannot be async, so this wait blocks the thread. The release
        // does not need the tokio runtime: `NbdHandle::drop` sends the
        // DISCONNECT from its own std thread, and the kernel does the rest.
        let deadline = Instant::now() + RELEASE_TIMEOUT;
        for device in &self.devices {
            while !kernel_released(device) {
                if Instant::now() >= deadline {
                    let (pid, size) = sysfs_state(device);
                    let message = format!(
                        "{} is still bound {RELEASE_TIMEOUT:?} after the test released it: \
                         sysfs pid {pid:?}, size {size:?}",
                        device.display(),
                    );
                    if panicking {
                        eprintln!("{message}");
                        break;
                    }
                    panic!("{message}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}
