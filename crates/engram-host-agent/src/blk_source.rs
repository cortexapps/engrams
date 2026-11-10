//! A read-only block device served from the fill engine: the source a
//! hydrating session volume copies its cold regions from
//! (`SessionDisk::hydrate`). The device is a ublk device the host-agent
//! serves in process; the kernel reads it for the regions the guest
//! touches before they are copied and for its background copy, and
//! nothing else opens it.
//!
//! Two kinds of read arrive and the source cannot be told which is
//! which, so it tells them apart by position. The background copy walks
//! the volume's cold regions in ascending order, one region or a batch
//! at a time; the source keeps a window of whole chunks around that
//! stream, fetched at `Hydrate` and a few chunks ahead, and hands them
//! out region by region, so the store sees one GET per chunk and the
//! copy runs at the engine's rate. A read anywhere else is the guest
//! waiting (or a write the kernel first copies the region for): it gets
//! a ranged GET at `Demand`, and its chunk is recorded as touched. So
//! does a read inside the window that is not whole regions, which only
//! the guest sends, when the window's chunk is not in yet.
//!
//! A store error that can pass stalls the read until it does: an I/O
//! error on a guest's root disk outlives the outage that caused it.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use engram_chunk_store::{ChunkHash, ChunkStoreError, Manifest};
use engram_core::error::BlobError;
use engram_fill::{Class, FillEngine};
use engram_host_core::{SourceDevice, SourceServer};
use io_uring::{opcode, types};
use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::io::{UblkDev, UblkIOCtx, UblkQueue};
use libublk::{sys, BufDesc, BufDescList, UblkError, UblkFlags, UblkIORes};
use tokio::sync::{mpsc, oneshot, OnceCell};

/// Chunks fetched ahead of the stream chunk. With 16 MiB chunks this is
/// 64 MiB in flight per device, which keeps the copy near the engine's
/// rate; the window holds at most this many plus two chunks.
const WINDOW_AHEAD: usize = 4;
/// Region-shaped reads at ascending delta chunks, outside the window,
/// that move the stream there (a host-agent restart loses the cursor).
const RESYNC_READS: u32 = 4;
/// A stream chunk's fetch still waiting at `Hydrate` after this long is
/// asked for again at `Demand`: a guest read of whole regions inside the
/// window waits on it, and `Hydrate` can wait behind other sessions.
const STREAM_PATIENCE: Duration = Duration::from_secs(1);
/// The wait after a failed fetch: from the first, doubling to the last.
const RETRY_FIRST: Duration = Duration::from_millis(100);
const RETRY_LAST: Duration = Duration::from_secs(5);
/// The tag the eventfd read carries; real tags are below the depth.
const EVENT_TAG: u16 = u16::MAX;
const QUEUE_DEPTH: u16 = 64;
/// The largest read the kernel sends; the block layer splits above it.
const IO_BUF_BYTES: u32 = 512 << 10;
const DEVICE_NODE_WAIT: Duration = Duration::from_secs(5);
/// Entries in a thread's ublk control ring; commands go one at a time.
const CTRL_RING_DEPTH: u32 = 16;

/// A served device. [`Self::stop`] ends it; a dropped one keeps serving
/// until the process exits, after which [`Self::recover`] takes it over.
pub struct BlkSource {
    ctrl: Arc<UblkCtrl>,
    path: PathBuf,
    shared: Arc<Shared>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl BlkSource {
    /// Serve `manifest` through a new device. `delta` lists, ascending,
    /// the chunks the kernel will copy (the ones not skipped), which is
    /// where the stream runs; `region_bytes` is the kernel's copy unit.
    pub async fn serve(
        engine: FillEngine,
        manifest: &Manifest,
        delta: Vec<u64>,
        region_bytes: u64,
    ) -> io::Result<Self> {
        Self::start(engine, manifest, delta, region_bytes, None).await
    }

    /// Take over device `dev_id` from a host-agent that died: the reads it
    /// left queued are reissued to this server.
    pub async fn recover(
        dev_id: u32,
        engine: FillEngine,
        manifest: &Manifest,
        delta: Vec<u64>,
        region_bytes: u64,
    ) -> io::Result<Self> {
        Self::start(engine, manifest, delta, region_bytes, Some(dev_id)).await
    }

    async fn start(
        engine: FillEngine,
        manifest: &Manifest,
        delta: Vec<u64>,
        region_bytes: u64,
        recover: Option<u32>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared::new(
            engine,
            manifest,
            delta,
            region_bytes,
            recover.is_none(),
        )?);
        let builder = UblkCtrlBuilder::default()
            .name("engram-source")
            .nr_queues(1)
            .depth(QUEUE_DEPTH)
            .io_buf_bytes(IO_BUF_BYTES)
            .ctrl_flags((sys::UBLK_F_USER_RECOVERY | sys::UBLK_F_USER_RECOVERY_REISSUE) as u64);
        let builder = match recover {
            None => builder.dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV),
            Some(id) => builder
                .id(id as i32)
                .dev_flags(UblkFlags::UBLK_DEV_F_RECOVER_DEV),
        };
        let ctrl = Arc::new(builder.build().map_err(ublk_err)?);
        if recover.is_some() {
            let ctrl = ctrl.clone();
            tokio::task::spawn_blocking(move || with_ctrl_ring(|| ctrl.start_user_recover()))
                .await
                .map_err(io::Error::other)??;
        }
        let (requests, queue) = mpsc::unbounded_channel();
        let completions = Arc::new(Completions::new()?);
        tokio::spawn(dispatch(shared.clone(), queue, completions.clone()));

        let (ready, started) = oneshot::channel();
        let server = std::thread::Builder::new()
            .name("engram-source".into())
            .spawn({
                let ctrl = ctrl.clone();
                let shared = shared.clone();
                move || {
                    let size_bytes = shared.size_bytes;
                    let ready = Arc::new(Mutex::new(Some(ready)));
                    let queue_fn = {
                        let shared = shared.clone();
                        move |qid: u16, dev: &UblkDev| {
                            queue_loop(qid, dev, &shared, &requests, &completions)
                        }
                    };
                    let run = with_ctrl_ring(|| {
                        ctrl.run_target(
                            |dev| {
                                dev.set_default_params(size_bytes);
                                dev.tgt.params.basic.attrs |= sys::UBLK_ATTR_READ_ONLY;
                                Ok(())
                            },
                            queue_fn,
                            {
                                let ready = ready.clone();
                                move |_| {
                                    if let Some(ready) = ready.lock().unwrap().take() {
                                        let _ = ready.send(Ok(()));
                                    }
                                }
                            },
                        )
                    });
                    if let Err(e) = run {
                        match ready.lock().unwrap().take() {
                            Some(ready) => {
                                let _ = ready.send(Err(e));
                            }
                            None => {
                                tracing::warn!(error = %e, "ublk source server ended with an error")
                            }
                        }
                    }
                }
            })?;
        started.await.map_err(|_| {
            io::Error::other("the ublk server thread ended before the device started")
        })??;
        let path = PathBuf::from(ctrl.get_bdev_path());
        wait_for_node(&path).await?;
        Ok(Self {
            ctrl,
            path,
            shared,
            server: Some(server),
        })
    }

    /// The block device: `/dev/ublkb<id>`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn dev_id(&self) -> u32 {
        self.ctrl.dev_info().dev_id
    }

    /// Chunks the guest read before the copy reached them, in first-read
    /// order.
    pub fn touched(&self) -> Vec<u32> {
        self.shared.touched.lock().unwrap().order.clone()
    }

    /// Stop serving and delete the device. Refused while a device still
    /// maps onto it: a device with reads queued and no server to answer
    /// them holds the kernel until reboot, so the mapping goes first.
    pub async fn stop(mut self) -> io::Result<()> {
        let holders = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| format!("/sys/block/{n}/holders"));
        if let Some(holders) = holders {
            let held = std::fs::read_dir(&holders)?.count();
            if held > 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    format!(
                        "{} is still the source of {held} device(s)",
                        self.path.display()
                    ),
                ));
            }
        }
        self.shared.stopping.store(true, Ordering::Relaxed);
        let ctrl = self.ctrl.clone();
        let server = self.server.take();
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            with_ctrl_ring(|| ctrl.kill_dev())?;
            if let Some(server) = server {
                server
                    .join()
                    .map_err(|_| io::Error::other("the ublk server thread panicked"))?;
            }
            with_ctrl_ring(|| ctrl.del_dev())?;
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }
}

impl Drop for BlkSource {
    fn drop(&mut self) {
        if self.server.is_some() {
            tracing::warn!(path = %self.path.display(), "ublk source dropped while serving; it serves until the process exits");
        }
    }
}

/// What the engine side and the queue thread share.
struct Shared {
    engine: FillEngine,
    /// The manifest's hash per chunk; `None` reads as zeros.
    hashes: Vec<Option<ChunkHash>>,
    chunk_size: u64,
    size_bytes: u64,
    region_bytes: u64,
    /// Ascending chunks the kernel copies.
    delta: Vec<u64>,
    window: Mutex<Window>,
    touched: Mutex<Touched>,
    stopping: AtomicBool,
}

struct Window {
    /// Position in `delta` of the chunk the kernel's copy is in; `None`
    /// until the stream shows itself.
    cursor: Option<usize>,
    chunks: HashMap<u64, Arc<OnceCell<Bytes>>>,
    /// The last region-shaped read outside the window and how many came
    /// in ascending order.
    resync: Option<(u64, u32)>,
}

struct Touched {
    order: Vec<u32>,
    seen: Vec<bool>,
}

enum Serve {
    Stream(Arc<OnceCell<Bytes>>),
    Demand,
}

impl Shared {
    fn new(
        engine: FillEngine,
        manifest: &Manifest,
        delta: Vec<u64>,
        region_bytes: u64,
        fresh: bool,
    ) -> io::Result<Self> {
        let chunk_size = manifest.chunk_size.0;
        if region_bytes == 0
            || chunk_size == 0
            || !chunk_size.is_multiple_of(region_bytes)
            || !manifest.total_bytes.is_multiple_of(512)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "manifest of {} bytes in {chunk_size}-byte chunks does not fit {region_bytes}-byte regions",
                    manifest.total_bytes
                ),
            ));
        }
        let count = manifest.expected_chunk_count() as usize;
        let mut hashes = vec![None; count];
        for chunk in &manifest.chunks {
            let index = (chunk.offset / chunk_size) as usize;
            match hashes.get_mut(index) {
                Some(slot) if chunk.offset.is_multiple_of(chunk_size) => *slot = Some(chunk.hash),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "manifest chunk at offset {} lies outside the volume",
                            chunk.offset
                        ),
                    ))
                }
            }
        }
        if !delta.is_sorted() || delta.last().is_some_and(|last| *last as usize >= count) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "delta chunks are not ascending indices of the volume",
            ));
        }
        let shared = Self {
            engine,
            hashes,
            chunk_size,
            size_bytes: manifest.total_bytes,
            region_bytes,
            delta,
            window: Mutex::new(Window {
                cursor: None,
                chunks: HashMap::new(),
                resync: None,
            }),
            touched: Mutex::new(Touched {
                order: Vec::new(),
                seen: vec![false; count],
            }),
            stopping: AtomicBool::new(false),
        };
        // A new device's copy starts at the first delta chunk; fetch it
        // and the chunks after it before the kernel asks.
        if fresh && !shared.delta.is_empty() {
            let mut window = shared.window.lock().unwrap();
            shared.advance(&mut window, 0);
        }
        Ok(shared)
    }

    async fn read(&self, offset: u64, len: u64) -> io::Result<Bytes> {
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= self.size_bytes)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "read of {len} at {offset} lies outside the {}-byte device",
                        self.size_bytes
                    ),
                )
            })?;
        let mut out = BytesMut::with_capacity(len as usize);
        let mut at = offset;
        while at < end {
            let chunk = at / self.chunk_size;
            let in_chunk = at % self.chunk_size;
            let piece = (self.chunk_size - in_chunk).min(end - at);
            match self.hashes[chunk as usize] {
                None => out.resize(out.len() + piece as usize, 0),
                Some(hash) => {
                    let regions = at.is_multiple_of(self.region_bytes)
                        && piece.is_multiple_of(self.region_bytes);
                    let bytes = match self.classify(chunk, at, piece) {
                        Serve::Stream(cell) if regions || cell.initialized() => {
                            let whole = self
                                .until_fetched(chunk, || async {
                                    cell.get_or_try_init(|| stream_chunk(&self.engine, hash))
                                        .await
                                        .cloned()
                                })
                                .await?;
                            whole.slice(in_chunk as usize..(in_chunk + piece) as usize)
                        }
                        Serve::Stream(_) | Serve::Demand => {
                            self.touch(chunk);
                            self.until_fetched(chunk, || {
                                self.engine.read_range(
                                    hash,
                                    in_chunk,
                                    piece as usize,
                                    Class::Demand,
                                )
                            })
                            .await?
                        }
                    };
                    if bytes.len() as u64 != piece {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "chunk {chunk} gave {} bytes for a {piece}-byte read",
                                bytes.len()
                            ),
                        ));
                    }
                    out.extend_from_slice(&bytes);
                }
            }
            at += piece;
        }
        Ok(out.freeze())
    }

    /// `fetch`, again after each error that can pass, until the device
    /// stops. A chunk the store does not hold, or holds with other
    /// bytes, fails at once.
    async fn until_fetched<F, Fut>(&self, chunk: u64, fetch: F) -> io::Result<Bytes>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<Bytes, ChunkStoreError>>,
    {
        let mut wait = RETRY_FIRST;
        loop {
            let error = match fetch().await {
                Ok(bytes) => return Ok(bytes),
                Err(error) => error,
            };
            let lasting = matches!(
                error,
                ChunkStoreError::Blob(BlobError::NotFound | BlobError::RangeOutOfBounds { .. })
                    | ChunkStoreError::HashMismatch { .. }
                    | ChunkStoreError::RangeOutOfBounds { .. }
            );
            if lasting || self.stopping.load(Ordering::Relaxed) {
                return Err(io::Error::other(error.to_string()));
            }
            tracing::warn!(chunk, %error, retry_ms = wait.as_millis() as u64, "ublk source: fetch failed; retrying");
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(RETRY_LAST);
        }
    }

    /// Stream or demand, by position against the window. A stream read
    /// at the chunk after the cursor moves the cursor; the chunk before
    /// it still serves, since the copy's reads of one chunk and the next
    /// overlap in flight.
    fn classify(&self, chunk: u64, at: u64, len: u64) -> Serve {
        let mut window = self.window.lock().unwrap();
        let near = window.cursor.and_then(|pos| {
            let before = pos.checked_sub(1).map(|p| self.delta[p]);
            if chunk == self.delta[pos] || Some(chunk) == before {
                Some(pos)
            } else if self.delta.get(pos + 1) == Some(&chunk) {
                Some(pos + 1)
            } else {
                None
            }
        });
        let pos = match near {
            Some(pos) => {
                window.resync = None;
                pos
            }
            None => {
                let shaped =
                    at.is_multiple_of(self.region_bytes) && len.is_multiple_of(self.region_bytes);
                match self.resync(&mut window, chunk, shaped) {
                    Some(pos) => pos,
                    None => return Serve::Demand,
                }
            }
        };
        self.advance(&mut window, pos);
        Serve::Stream(window.chunks.entry(chunk).or_default().clone())
    }

    /// A region-shaped read of a delta chunk outside the window: the
    /// first one places a stream the device has not seen yet; otherwise
    /// `RESYNC_READS` of them in ascending order move the stream there.
    fn resync(&self, window: &mut Window, chunk: u64, shaped: bool) -> Option<usize> {
        if !shaped {
            return None;
        }
        let pos = self.delta.binary_search(&chunk).ok()?;
        let run = match window.resync {
            Some((last, run)) if chunk >= last => run + 1,
            _ => 1,
        };
        window.resync = Some((chunk, run));
        if window.cursor.is_none() || run >= RESYNC_READS {
            window.resync = None;
            window.chunks.clear();
            return Some(pos);
        }
        None
    }

    /// Put the cursor at `pos`, drop the chunks behind the one before it,
    /// and fetch the chunks up to `WINDOW_AHEAD` past it that are not in
    /// the window yet.
    fn advance(&self, window: &mut Window, pos: usize) {
        if window.cursor != Some(pos) {
            window.cursor = Some(pos);
            let keep_from = pos.checked_sub(1).map_or(0, |p| self.delta[p]);
            window.chunks.retain(|chunk, _| *chunk >= keep_from);
        }
        let last = (pos + WINDOW_AHEAD).min(self.delta.len() - 1);
        for chunk in self.delta[pos..=last].iter().copied() {
            let Some(hash) = self.hashes[chunk as usize] else {
                continue;
            };
            if let std::collections::hash_map::Entry::Vacant(slot) = window.chunks.entry(chunk) {
                let cell = slot.insert(Arc::default()).clone();
                let engine = self.engine.clone();
                tokio::spawn(async move {
                    if let Err(e) = cell.get_or_try_init(|| stream_chunk(&engine, hash)).await {
                        tracing::debug!(chunk, error = %e, "stream prefetch failed; the read retries it");
                    }
                });
            }
        }
    }

    fn touch(&self, chunk: u64) {
        let mut touched = self.touched.lock().unwrap();
        if !std::mem::replace(&mut touched.seen[chunk as usize], true) {
            touched.order.push(chunk as u32);
        }
    }
}

/// A whole verified chunk for the stream window: at `Hydrate`, and at
/// `Demand` too once that has waited `STREAM_PATIENCE`.
async fn stream_chunk(engine: &FillEngine, hash: ChunkHash) -> Result<Bytes, ChunkStoreError> {
    let hydrate = engine.read(hash, Class::Hydrate);
    tokio::pin!(hydrate);
    tokio::select! {
        biased;
        read = &mut hydrate => return read,
        () = tokio::time::sleep(STREAM_PATIENCE) => {}
    }
    tokio::select! {
        read = &mut hydrate => read,
        read = engine.read(hash, Class::Demand) => read,
    }
}

/// Whether this host can serve a source: the ublk control device is
/// there and opens. Without it a session volume is written in full
/// before its guest starts.
pub fn available() -> io::Result<()> {
    File::options()
        .read(true)
        .write(true)
        .open("/dev/ublk-control")
        .map(drop)
}

struct Request {
    tag: u16,
    offset: u64,
    len: u32,
}

/// Finished reads on their way back to the queue thread, which sleeps
/// in `io_uring_enter`; the eventfd wakes it.
struct Completions {
    done: Mutex<VecDeque<(u16, io::Result<Bytes>)>>,
    event: File,
}

impl Completions {
    fn new() -> io::Result<Self> {
        // SAFETY: eventfd takes two integers and returns a new descriptor
        // or -1; nothing is borrowed.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor this process owns and nothing
        // else closes.
        let event = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        Ok(Self {
            done: Mutex::new(VecDeque::new()),
            event,
        })
    }

    fn push(&self, tag: u16, result: io::Result<Bytes>) {
        self.done.lock().unwrap().push_back((tag, result));
        if let Err(e) = (&self.event).write_all(&1u64.to_ne_bytes()) {
            tracing::error!(error = %e, "ublk source: eventfd write failed; a read stays unanswered");
        }
    }

    fn drain(&self) -> Vec<(u16, io::Result<Bytes>)> {
        self.done.lock().unwrap().drain(..).collect()
    }
}

async fn dispatch(
    shared: Arc<Shared>,
    mut requests: mpsc::UnboundedReceiver<Request>,
    completions: Arc<Completions>,
) {
    while let Some(Request { tag, offset, len }) = requests.recv().await {
        let shared = shared.clone();
        let completions = completions.clone();
        tokio::spawn(async move {
            let result = shared.read(offset, len as u64).await;
            completions.push(tag, result);
        });
    }
}

/// The queue thread: hands each read to the engine side and completes
/// it when the eventfd says its bytes are back. Runs until the device
/// stops.
fn queue_loop(
    qid: u16,
    dev: &UblkDev,
    shared: &Shared,
    requests: &mpsc::UnboundedSender<Request>,
    completions: &Completions,
) {
    let bufs = dev.alloc_queue_io_bufs();
    let queue = match UblkQueue::new(qid, dev)
        .and_then(|q| q.submit_fetch_commands_unified(BufDescList::Slices(Some(&bufs))))
    {
        Ok(queue) => queue,
        Err(e) => {
            tracing::error!(qid, error = %e, "ublk source queue did not start");
            return;
        }
    };
    let mut counter = 0u64;
    arm_event(&queue, &completions.event, &mut counter);
    queue.wait_and_handle_io(|queue, tag, io| {
        if io.is_tgt_io() {
            for (tag, result) in completions.drain() {
                let buf = &bufs[tag as usize];
                let res = match result {
                    Ok(bytes) if bytes.len() <= buf.len() => {
                        // SAFETY: the buffer is `buf.len()` bytes; between
                        // this request's arrival and its commit below, this
                        // thread is its only writer and the kernel does not
                        // read it.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                bytes.as_ptr(),
                                buf.as_mut_ptr(),
                                bytes.len(),
                            );
                        }
                        bytes.len() as i32
                    }
                    Ok(bytes) => {
                        tracing::error!(
                            tag,
                            len = bytes.len(),
                            "ublk source: read answered with more than its buffer"
                        );
                        -libc::EIO
                    }
                    Err(e) => {
                        tracing::warn!(tag, error = %e, "ublk source: read failed");
                        -libc::EIO
                    }
                };
                complete(queue, tag, buf.as_slice(), res);
            }
            arm_event(queue, &completions.event, &mut counter);
            return;
        }
        let iod = queue.get_iod(tag);
        let buf = &bufs[tag as usize];
        let op = iod.op_flags & 0xff;
        if op != sys::UBLK_IO_OP_READ {
            complete(queue, tag, buf.as_slice(), -libc::EOPNOTSUPP);
            return;
        }
        if shared.stopping.load(Ordering::Relaxed) {
            complete(queue, tag, buf.as_slice(), -libc::EIO);
            return;
        }
        let request = Request {
            tag,
            offset: iod.start_sector << 9,
            len: iod.nr_sectors << 9,
        };
        // No receiver: the runtime is shutting down with the process. The
        // read stays unanswered, and the kernel gives it to the next
        // server (`UBLK_F_USER_RECOVERY_REISSUE`); an error here would
        // reach the guest.
        let _ = requests.send(request);
    });
}

fn complete(queue: &UblkQueue, tag: u16, buf: &[u8], res: i32) {
    if let Err(e) =
        queue.complete_io_cmd_unified(tag, BufDesc::Slice(buf), Ok(UblkIORes::Result(res)))
    {
        tracing::error!(tag, error = %e, "ublk source: completion not submitted");
    }
}

/// Queue a read of the eventfd so a completion wakes the queue thread.
fn arm_event(queue: &UblkQueue, event: &File, counter: &mut u64) {
    let sqe = opcode::Read::new(
        types::Fd(event.as_raw_fd()),
        std::ptr::from_mut(counter).cast::<u8>(),
        std::mem::size_of::<u64>() as u32,
    )
    .build()
    .user_data(UblkIOCtx::build_user_data(EVENT_TAG, 0, 0, true));
    if let Err(e) = queue.ublk_submit_sqe_sync(sqe) {
        tracing::error!(error = %e, "ublk source: eventfd read not submitted; completions stall");
    }
}

async fn wait_for_node(path: &Path) -> io::Result<()> {
    let appears = async {
        while tokio::fs::metadata(path).await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(DEVICE_NODE_WAIT, appears)
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} did not appear", path.display()),
            )
        })
}

fn ublk_err(e: UblkError) -> io::Error {
    match e {
        UblkError::IOError(e) => e,
        other => io::Error::other(other.to_string()),
    }
}

/// Run `f`, a libublk control command, on this thread. libublk sends
/// control commands over a thread-local ring that only the thread which
/// built the `UblkCtrl` has; every other thread makes its own first.
fn with_ctrl_ring<T>(f: impl FnOnce() -> Result<T, UblkError>) -> io::Result<T> {
    libublk::ublk_init_ctrl_task_ring(|ring| {
        if ring.is_none() {
            *ring = Some(
                io_uring::IoUring::<io_uring::squeue::Entry128>::builder()
                    .build(CTRL_RING_DEPTH)
                    .map_err(UblkError::IOError)?,
            );
        }
        Ok(())
    })
    .map_err(ublk_err)?;
    f().map_err(ublk_err)
}

/// The host's [`SourceServer`]: every source is a [`BlkSource`].
pub struct UblkSources;

#[async_trait]
impl SourceServer for UblkSources {
    async fn serve(
        &self,
        fill: FillEngine,
        manifest: &Manifest,
        delta: Vec<u64>,
        region_bytes: u64,
    ) -> io::Result<Box<dyn SourceDevice>> {
        Ok(Box::new(
            BlkSource::serve(fill, manifest, delta, region_bytes).await?,
        ))
    }

    async fn recover(
        &self,
        path: &Path,
        fill: FillEngine,
        manifest: &Manifest,
        delta: Vec<u64>,
        region_bytes: u64,
    ) -> io::Result<Box<dyn SourceDevice>> {
        Ok(Box::new(
            BlkSource::recover(dev_id_of(path)?, fill, manifest, delta, region_bytes).await?,
        ))
    }
}

#[async_trait]
impl SourceDevice for BlkSource {
    fn path(&self) -> &Path {
        BlkSource::path(self)
    }

    fn touched(&self) -> Vec<u32> {
        BlkSource::touched(self)
    }

    async fn stop(self: Box<Self>) -> io::Result<()> {
        BlkSource::stop(*self).await
    }
}

/// The id in `/dev/ublkb<id>`.
fn dev_id_of(path: &Path) -> io::Result<u32> {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("ublkb"))
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a ublk block device", path.display()),
            )
        })
}

/// Delete every ublk device nothing maps onto and no server in this
/// process serves (`served` lists their paths): what a process left
/// when it died between making a device and mapping a volume onto it.
/// A device a volume still maps onto stays, with a warning: deleting it
/// would wait on reads nothing answers.
pub async fn reap_orphans(served: &[PathBuf]) -> io::Result<Vec<u32>> {
    let mut reaped = Vec::new();
    let mut entries = match tokio::fs::read_dir("/dev").await {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(reaped),
        Err(e) => return Err(e),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let Ok(id) = dev_id_of(&path) else { continue };
        if served.contains(&path) {
            continue;
        }
        let held = std::fs::read_dir(format!("/sys/block/ublkb{id}/holders"))
            .map(|holders| holders.count())
            .unwrap_or(0);
        if held > 0 {
            tracing::warn!(device = %path.display(), holders = held, "an unserved ublk device is still mapped; it is not deleted");
            continue;
        }
        tokio::task::spawn_blocking(move || {
            with_ctrl_ring(|| UblkCtrl::new_simple(id as i32)?.del_dev())
        })
        .await
        .map_err(io::Error::other)??;
        tracing::info!(device = %path.display(), "deleted an orphaned ublk device");
        reaped.push(id);
    }
    Ok(reaped)
}
