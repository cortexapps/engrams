//! The ublk server: a read-only block device over a [`DiskReader`].
//!
//! The device is created with user recovery and reissue. When the
//! process dies, the kernel keeps `/dev/ublkbN`, holds its I/O, and
//! gives the requests in flight again to the next process that recovers
//! it. Only a normal stop deletes the device.
//!
//! Each queue thread runs one task per tag on a local executor driven by
//! the queue's io_uring. A read runs on the tokio runtime, and the tag's
//! task waits for it on an eventfd polled through the same ring, so a
//! slow read holds only its own tag.

use std::future::Future;
use std::io;
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use io_uring::{opcode, types};
use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::helpers::IoBuf;
use libublk::io::{UblkDev, UblkQueue};
use libublk::uring_async::ublk_submit_sqe_async;
use libublk::{sys, BufDesc, UblkError, UblkFlags, UblkUringData};
use nix::sys::eventfd::{EfdFlags, EventFd};
use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};

use crate::reader::DiskReader;

/// The device's logical and physical block size.
const BLOCK_SHIFT: u8 = 12;
const BLOCK_BYTES: u64 = 1 << BLOCK_SHIFT;
/// The largest single request the kernel sends.
const IO_BUF_BYTES: u32 = 512 * 1024;
/// The longest wait between attempts at a read that fails.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);

/// How the device is shaped and whether it is new.
#[derive(Clone, Debug)]
pub struct Config {
    pub queues: u16,
    pub depth: u16,
    /// Attach to this existing device instead of creating one.
    pub recover: Option<u32>,
}

/// The device a server serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub dev_id: u32,
    pub path: String,
}

fn ublk_err(what: &str, e: UblkError) -> io::Error {
    match e {
        UblkError::IOError(e) => io::Error::new(e.kind(), format!("{what}: {e}")),
        UblkError::UringIOError(errno) | UblkError::OtherError(errno) if errno < 0 => {
            let os = io::Error::from_raw_os_error(-errno);
            io::Error::new(os.kind(), format!("{what}: {os}"))
        }
        other => io::Error::other(format!("{what}: {other}")),
    }
}

/// The control commands report EBUSY as a success value.
fn check(what: &str, res: Result<i32, UblkError>) -> io::Result<i32> {
    match res {
        Ok(r) if r < 0 => Err(ublk_err(what, UblkError::OtherError(r))),
        Ok(r) => Ok(r),
        Err(e) => Err(ublk_err(what, e)),
    }
}

/// Serve `reader` as a ublk device until `stop` completes, then delete
/// the device. `on_ready` runs once the device serves I/O. A process
/// that exits any other way leaves the device for [`Config::recover`].
pub fn serve<R, S>(
    reader: Arc<DiskReader>,
    rt: Handle,
    config: &Config,
    on_ready: R,
    stop: S,
) -> io::Result<()>
where
    R: FnOnce(&Device) + Send + Sync + 'static,
    S: Future<Output = ()> + Send + 'static,
{
    let size = reader.size_bytes();
    if size == 0 || !size.is_multiple_of(BLOCK_BYTES) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a disk of {size} bytes is not a whole number of {BLOCK_BYTES} byte blocks"),
        ));
    }
    let ctrl = open_ctrl(config, size)?;

    let tgt_init = move |dev: &mut UblkDev| {
        let depth = dev.dev_info.queue_depth;
        // One request command and one eventfd poll per tag.
        dev.tgt.sq_depth = depth.saturating_mul(2);
        dev.tgt.cq_depth = depth.saturating_mul(2);
        dev.tgt.dev_size = size;
        dev.tgt.params = params(size, dev.dev_info.max_io_buf_bytes);
        Ok(())
    };
    let stop_rt = rt.clone();
    // The kernel stops a device only once every request in flight has
    // completed, so a stop first ends the reads still trying again.
    let (stopping_tx, stopping) = watch::channel(false);
    let serve_queue = move |qid: u16, dev: &UblkDev| {
        serve_queue(qid, dev, &reader, &rt, &stopping);
    };
    // `run_target` wants a `Sync` closure; the stop future is only `Send`.
    let stop = Mutex::new(Some(stop));
    let on_started = move |ctrl: &UblkCtrl| {
        on_ready(&Device {
            dev_id: ctrl.dev_info().dev_id,
            path: ctrl.get_bdev_path(),
        });
        if let Some(stop) = stop.lock().ok().and_then(|mut s| s.take()) {
            stop_rt.block_on(stop);
        }
        let _ = stopping_tx.send(true);
        // Stops the queues; `run_target` then returns.
        if let Err(e) = ctrl.kill_dev() {
            tracing::warn!(error = %e, "stop the device");
        }
    };
    check(
        "serve the device",
        ctrl.run_target(tgt_init, serve_queue, on_started),
    )?;
    check("delete the device", ctrl.del_dev())?;
    Ok(())
}

fn params(size: u64, max_io_buf_bytes: u32) -> sys::ublk_params {
    sys::ublk_params {
        types: sys::UBLK_PARAM_TYPE_BASIC,
        basic: sys::ublk_param_basic {
            attrs: sys::UBLK_ATTR_READ_ONLY,
            logical_bs_shift: BLOCK_SHIFT,
            physical_bs_shift: BLOCK_SHIFT,
            io_opt_shift: BLOCK_SHIFT,
            io_min_shift: BLOCK_SHIFT,
            max_sectors: max_io_buf_bytes >> 9,
            dev_sectors: size >> 9,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn open_ctrl(config: &Config, size: u64) -> io::Result<UblkCtrl> {
    let builder = UblkCtrlBuilder::default()
        .name("engram-disk-source")
        .nr_queues(config.queues)
        .depth(config.depth)
        .io_buf_bytes(IO_BUF_BYTES)
        .ctrl_flags((sys::UBLK_F_USER_RECOVERY | sys::UBLK_F_USER_RECOVERY_REISSUE) as u64);
    let Some(dev_id) = config.recover else {
        return builder
            .id(-1)
            .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
            .build()
            .map_err(|e| ublk_err("create the device", e));
    };
    let id = i32::try_from(dev_id).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("device id {dev_id} is out of range"),
        )
    })?;
    let ctrl = builder
        .id(id)
        .dev_flags(UblkFlags::UBLK_DEV_F_RECOVER_DEV)
        .build()
        .map_err(|e| ublk_err(&format!("open device {dev_id}"), e))?;
    if ctrl.dev_info().flags & sys::UBLK_F_USER_RECOVERY as u64 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("device {dev_id} was not created for recovery"),
        ));
    }
    let mut kernel = sys::ublk_params::default();
    check("read the device parameters", ctrl.get_params(&mut kernel))?;
    if kernel.basic.dev_sectors != size >> 9
        || kernel.basic.attrs & sys::UBLK_ATTR_READ_ONLY == 0
        || kernel.basic.logical_bs_shift != BLOCK_SHIFT
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "device {dev_id} is a {} byte device; this source is a read-only {size} byte disk",
                kernel.basic.dev_sectors << 9
            ),
        ));
    }
    check(
        &format!("start recovery of device {dev_id}"),
        ctrl.start_user_recover(),
    )?;
    Ok(ctrl)
}

struct Tag<'a> {
    reader: Arc<DiskReader>,
    rt: Handle,
    stopping: watch::Receiver<bool>,
    queue: Rc<UblkQueue<'a>>,
    tag: u16,
}

fn serve_queue(
    qid: u16,
    dev: &UblkDev,
    reader: &Arc<DiskReader>,
    rt: &Handle,
    stopping: &watch::Receiver<bool>,
) {
    let queue = match UblkQueue::new(qid, dev) {
        Ok(queue) => Rc::new(queue),
        Err(e) => {
            tracing::error!(qid, error = %e, "open the queue");
            return;
        }
    };
    let exe = Rc::new(smol::LocalExecutor::new());
    let tasks: Vec<_> = queue
        .tags()
        .map(|tag| {
            let tag = Tag {
                reader: Arc::clone(reader),
                rt: rt.clone(),
                stopping: stopping.clone(),
                queue: Rc::clone(&queue),
                tag,
            };
            exe.spawn(async move {
                let id = tag.tag;
                match serve_tag(tag).await {
                    Ok(()) | Err(UblkError::QueueIsDown) => {}
                    Err(e) => tracing::error!(qid, tag = id, error = %e, "tag stopped serving"),
                }
            })
        })
        .collect();
    let ticker = Rc::clone(&exe);
    smol::block_on(exe.run(async {
        let run_ops = || while ticker.try_tick() {};
        let is_done = || tasks.iter().all(|task| task.is_finished());
        match libublk::wait_and_handle_io_events(&queue, Some(20), run_ops, is_done).await {
            Ok(()) | Err(UblkError::QueueIsDown) => {}
            Err(e) => tracing::error!(qid, error = %e, "queue event loop"),
        }
    }));
}

async fn serve_tag(t: Tag<'_>) -> Result<(), UblkError> {
    let (queue, tag) = (&*t.queue, t.tag);
    let done = Arc::new(
        EventFd::from_value_and_flags(0, EfdFlags::EFD_NONBLOCK | EfdFlags::EFD_CLOEXEC)
            .map_err(|e| UblkError::IOError(e.into()))?,
    );
    let mut buf = IoBuf::<u8>::new(queue.dev.dev_info.max_io_buf_bytes as usize);
    queue
        .submit_io_prep_cmd(tag, BufDesc::Slice(buf.as_slice()), 0, Some(&buf))
        .await?;
    loop {
        let res = handle_io(&t, buf.as_mut_slice(), &done).await;
        queue
            .submit_io_commit_cmd(tag, BufDesc::Slice(buf.as_slice()), res)
            .await?;
    }
}

/// The result the kernel gets for the request on `tag`: bytes read, or a
/// negative errno.
async fn handle_io(t: &Tag<'_>, buf: &mut [u8], done: &Arc<EventFd>) -> i32 {
    let iod = t.queue.get_iod(t.tag);
    match iod.op_flags & 0xff {
        sys::UBLK_IO_OP_READ => {
            let offset = iod.start_sector << 9;
            let len = (iod.nr_sectors as usize) << 9;
            if len > buf.len() {
                return -libc::EINVAL;
            }
            match read_on_runtime(t, offset, len, done).await {
                Ok(bytes) => {
                    buf[..len].copy_from_slice(&bytes);
                    len as i32
                }
                Err(e) => {
                    tracing::error!(offset, len, error = %e, "read refused");
                    if e.kind() == io::ErrorKind::InvalidInput {
                        -libc::EINVAL
                    } else {
                        -libc::EIO
                    }
                }
            }
        }
        // The device has no volatile cache, so the kernel sends no flush;
        // one that arrives has nothing to make durable.
        sys::UBLK_IO_OP_FLUSH => 0,
        _ => -libc::EROFS,
    }
}

/// Run the read on the tokio runtime and wait for it through the queue
/// ring: the runtime signals `done`, whose poll completes on the ring.
async fn read_on_runtime(
    t: &Tag<'_>,
    offset: u64,
    len: usize,
    done: &Arc<EventFd>,
) -> io::Result<Vec<u8>> {
    let (tx, mut rx) = oneshot::channel();
    let reader = Arc::clone(&t.reader);
    let stopping = t.stopping.clone();
    let signal = Arc::clone(done);
    t.rt.spawn(async move {
        let _ = tx.send(read_until_served(&reader, offset, len, stopping).await);
        if let Err(e) = signal.write(1) {
            tracing::error!(error = %e, "signal a finished read");
        }
    });
    loop {
        let poll = opcode::PollAdd::new(types::Fd(done.as_raw_fd()), libc::POLLIN as u32).build();
        let res = ublk_submit_sqe_async(poll, UblkUringData::Target as u64)
            .await
            .map_err(|e| ublk_err("wait for a read", e))?;
        if res < 0 {
            return Err(io::Error::from_raw_os_error(-res));
        }
        // Reset the counter before the next request reuses it.
        let _ = done.read();
        match rx.try_recv() {
            Ok(result) => return result,
            Err(oneshot::error::TryRecvError::Empty) => continue,
            Err(oneshot::error::TryRecvError::Closed) => {
                return Err(io::Error::other("the read task ended without a result"))
            }
        }
    }
}

/// A read that fails for a reason that may pass is attempted again until
/// it succeeds, so a slow or unreachable store stalls the guest's read
/// and never fails it. A chunk the store does not hold, or whose bytes
/// fail the hash check, is data loss: after one fresh fetch the read is
/// refused. A request outside the disk, or a read still waiting when the
/// server stops, is refused at once.
async fn read_until_served(
    reader: &DiskReader,
    offset: u64,
    len: usize,
    mut stopping: watch::Receiver<bool>,
) -> io::Result<Vec<u8>> {
    let stopped = || io::Error::other("the server is stopping");
    let mut delay = Duration::from_millis(50);
    let mut attempt = 1u32;
    let mut lost_once = false;
    loop {
        let result = tokio::select! {
            result = reader.read(offset, len) => result,
            _ = stopping.wait_for(|s| *s) => return Err(stopped()),
        };
        match result {
            Ok(bytes) => return Ok(bytes),
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => return Err(e),
            Err(e) if is_lost(&e) && lost_once => {
                tracing::error!(offset, len, error = %e, "disk source data lost: the chunk store cannot serve this read");
                return Err(e);
            }
            Err(e) if is_lost(&e) => {
                // The failed fetch left no cached copy, so this attempt
                // fetches again.
                tracing::warn!(offset, len, error = %e, "read failed for lost data; fetching once more");
                lost_once = true;
            }
            Err(e) => {
                tracing::error!(offset, len, attempt, error = %e, "read failed; trying again");
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = stopping.wait_for(|s| *s) => return Err(stopped()),
                }
                delay = (delay * 2).min(MAX_RETRY_DELAY);
                attempt += 1;
            }
        }
    }
}

fn is_lost(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::InvalidData
    )
}
