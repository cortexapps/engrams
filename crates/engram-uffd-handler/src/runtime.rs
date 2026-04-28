//! UFFD event loop. Linux-only — `userfaultfd(2)` is a Linux kernel
//! syscall and the upstream `userfaultfd` crate fails to compile on
//! other targets.
//!
//! Three small `unsafe` blocks live in this module; each is annotated
//! with a `// SAFETY:` comment justifying it. They are unavoidable
//! because the kernel surfaces (mmap, UFFDIO_COPY, taking ownership
//! of an fd received via SCM_RIGHTS) are inherently unsafe.

#![cfg(target_os = "linux")]

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::ptr;

use sendfd::RecvWithFd;
use userfaultfd::{Event, Uffd};

/// Strip O_NONBLOCK from `fd` so blocking reads on it actually block.
///
/// Firecracker creates the userfaultfd as O_NONBLOCK (it polls on its
/// own side). The `userfaultfd` crate's `read_event` translates EAGAIN
/// into `Ok(None)`, and our event loop's `Ok(None) => exit` arm
/// triggered after the *first* fault. Without this fix, the handler
/// served one page then quit, FC's vCPU faulted on the next
/// untouched page and blocked in `handle_userfault` forever.
fn make_blocking(fd: i32) -> std::io::Result<()> {
    // SAFETY: fd is a valid kernel fd we own (received via SCM_RIGHTS,
    // wrapped in Uffd which owns it). fcntl with F_GETFL/F_SETFL is
    // standard and side-effect-free on flag bits we don't touch.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let new_flags = flags & !libc::O_NONBLOCK;
    if unsafe { libc::fcntl(fd, libc::F_SETFL, new_flags) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

use crate::proto::{GuestRegionUffdMapping, HANDSHAKE_BUF_BYTES};

/// Anything that can go wrong during handler startup or while serving
/// faults. Kept as a unified error so `main.rs` can just bubble it.
#[derive(Debug)]
pub enum HandlerError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Uffd(userfaultfd::Error),
    /// SCM_RIGHTS payload didn't contain exactly one fd.
    UnexpectedFdCount(usize),
    /// A page fault landed at an address outside every known region.
    /// Should never happen in normal operation; if it does, our
    /// mappings drifted from Firecracker's. Surface as a hard error.
    AddressOutsideRegions(u64),
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Json(e) => write!(f, "json: {e}"),
            Self::Uffd(e) => write!(f, "uffd: {e}"),
            Self::UnexpectedFdCount(n) => {
                write!(f, "expected exactly 1 fd via SCM_RIGHTS, got {n}")
            }
            Self::AddressOutsideRegions(addr) => {
                write!(f, "page fault at {addr:#x} outside every registered region")
            }
        }
    }
}

impl std::error::Error for HandlerError {}

impl From<std::io::Error> for HandlerError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for HandlerError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

impl From<userfaultfd::Error> for HandlerError {
    fn from(e: userfaultfd::Error) -> Self {
        Self::Uffd(e)
    }
}

/// Receive Firecracker's handshake on `stream`: the JSON mappings
/// blob (data) plus the UFFD file descriptor (SCM_RIGHTS control msg).
/// Wraps the raw fd in a [`Uffd`].
///
/// Mirrors `UffdHandler::from_unix_stream` upstream.
pub fn recv_handshake(
    stream: &UnixStream,
) -> Result<(Vec<GuestRegionUffdMapping>, Uffd), HandlerError> {
    let mut buf = vec![0u8; HANDSHAKE_BUF_BYTES];
    let mut fds = [0i32; 1];
    let (bytes, fd_count) = stream.recv_with_fd(&mut buf, &mut fds)?;
    if fd_count != 1 {
        return Err(HandlerError::UnexpectedFdCount(fd_count));
    }
    buf.truncate(bytes);

    let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_slice(&buf)?;
    let raw_fd = fds[0];

    // SAFETY: `raw_fd` was just delivered to us by the kernel via
    // SCM_RIGHTS on an established UDS connection from Firecracker
    // which (per its source) only sends a valid userfaultfd descriptor
    // here. Ownership transfers to `Uffd`; we never touch `raw_fd`
    // again after this line.
    let uffd = unsafe { Uffd::from_raw_fd(raw_fd) };

    Ok((mappings, uffd))
}

/// All the state the event loop needs after the handshake.
///
/// We use a raw `libc::mmap` (PROT_READ | MAP_PRIVATE | MAP_POPULATE)
/// rather than `memmap2::Mmap` to match the upstream
/// `on_demand_handler.rs` byte-for-byte. `MAP_POPULATE` pre-faults
/// the host pages of memory.bin so the handler doesn't take its own
/// page faults while servicing the guest's.
pub struct Runtime {
    mappings: Vec<GuestRegionUffdMapping>,
    uffd: Uffd,
    backing_ptr: *const u8,
    backing_size: usize,
    // Hold the file open so the kernel doesn't drop our mmap backing.
    _file: File,
}

// SAFETY: backing_ptr is owned by this struct and only read; no mutable
// aliasing or threading concerns. We never expose `&mut` access.
unsafe impl Send for Runtime {}
unsafe impl Sync for Runtime {}

impl Runtime {
    /// Open `memory.bin`, mmap it read-only with MAP_POPULATE, and
    /// pair it with the UFFD the handshake gave us.
    pub fn new(
        memory_bin: &Path,
        mappings: Vec<GuestRegionUffdMapping>,
        uffd: Uffd,
    ) -> Result<Self, HandlerError> {
        let file = File::open(memory_bin).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("open memory.bin {}: {e}", memory_bin.display()),
            )
        })?;
        let size = file.metadata()?.len() as usize;

        // SAFETY: file is a regular file we just opened, fd is valid,
        // size matches its length. PROT_READ + MAP_PRIVATE means we
        // can't accidentally write to the snapshot. MAP_POPULATE
        // pre-faults the pages in our own address space, so when we
        // dereference `backing_ptr` to fill a guest fault we don't
        // ourselves block on a host-side page fault.
        let raw = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_READ,
                libc::MAP_PRIVATE | libc::MAP_POPULATE,
                file.as_raw_fd(),
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(HandlerError::Io(std::io::Error::last_os_error()));
        }

        Ok(Self {
            mappings,
            uffd,
            backing_ptr: raw as *const u8,
            backing_size: size,
            _file: file,
        })
    }

    /// Run forever, draining events from the UFFD and serving each
    /// page fault. Returns `Ok(())` cleanly when the UFFD is closed
    /// (Firecracker exited / sandbox destroyed).
    pub fn run(&self) -> Result<(), HandlerError> {
        let mut faults_served: u64 = 0;
        loop {
            match self.uffd.read_event() {
                Ok(Some(Event::Pagefault { addr, .. })) => {
                    self.serve_pagefault(addr as u64)?;
                    faults_served += 1;
                    if faults_served.is_power_of_two() {
                        // Log on 1, 2, 4, 8, 16, ... so a healthy
                        // restore shows progress without spamming.
                        tracing::info!(faults_served, "served page fault");
                    }
                }
                Ok(Some(other)) => {
                    tracing::debug!(?other, "non-pagefault uffd event");
                }
                Ok(None) => {
                    return Ok(());
                }
                Err(userfaultfd::Error::ReadEof) => {
                    tracing::info!(
                        faults_served,
                        "uffd closed by firecracker; handler exiting cleanly"
                    );
                    return Ok(());
                }
                Err(e) => return Err(HandlerError::Uffd(e)),
            }
        }
    }

    fn serve_pagefault(&self, fault_addr: u64) -> Result<(), HandlerError> {
        let region = self
            .mappings
            .iter()
            .find(|r| r.contains(fault_addr))
            .ok_or(HandlerError::AddressOutsideRegions(fault_addr))?;

        // Round down to the start of the page that faulted.
        let page_size = region.page_size;
        let dst_addr = fault_addr & !((page_size as u64) - 1);
        let intra = dst_addr - region.base_host_virt_addr;
        let file_offset = (region.offset + intra) as usize;

        // SAFETY: `backing_ptr` covers `backing_size` bytes; we ensure
        // the read range stays inside via `region.offset + intra`
        // which is bounded by `region.size` (the kernel-checked
        // GuestRegionUffdMapping). Out-of-bounds would be a
        // logic bug we'd catch with the assert below.
        debug_assert!(file_offset + page_size <= self.backing_size);
        let src = unsafe { self.backing_ptr.add(file_offset) };
        let dst = dst_addr as *mut std::ffi::c_void;

        // SAFETY: `src` points into our own mmapped backing buffer at
        // a valid offset. `dst` is the kernel's faulting address — it
        // owns the destination range. `len` matches the negotiated
        // page size. `wake=true` lets the guest's vCPU continue.
        unsafe {
            self.uffd.copy(src as *const _, dst, page_size, true)?;
        }
        Ok(())
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: backing_ptr was returned by libc::mmap on creation
        // and we hold the only reference to it; munmap with the same
        // size is the matching cleanup.
        unsafe {
            libc::munmap(self.backing_ptr as *mut _, self.backing_size);
        }
    }
}

// Stand-alone block to keep impl Runtime contiguous above without
// breaking the file structure when adding Drop.
impl Runtime {
    /// Test/debug accessor.
    #[allow(dead_code)]
    pub fn mappings(&self) -> &[GuestRegionUffdMapping] {
        &self.mappings
    }
}

/// One-shot run: bind the listener, accept Firecracker, handshake,
/// then loop until the UFFD closes.
///
/// We deliberately keep `stream` alive for the lifetime of the run
/// — matches the upstream `on_demand_handler.rs` which polls both
/// the stream and the UFFD. Dropping the stream after the handshake
/// caused Firecracker to hang on `PUT /snapshot/load` (likely
/// because FC keeps its side open and treats our close as a
/// protocol error).
pub fn run_listener(listen: PathBuf, memory_bin: PathBuf) -> Result<(), HandlerError> {
    let pid = std::process::id();
    let _ = std::fs::remove_file(&listen);
    let listener = std::os::unix::net::UnixListener::bind(&listen)?;
    tracing::info!(pid, socket = %listen.display(), "engram-uffd-handler listening");
    let (stream, _addr) = listener.accept()?;
    tracing::info!(pid, "Firecracker connected; awaiting handshake");
    let (mappings, uffd) = recv_handshake(&stream)?;
    let uffd_fd = std::os::fd::AsRawFd::as_raw_fd(&uffd);
    make_blocking(uffd_fd)?;
    let total: usize = mappings.iter().map(|m| m.size).sum();
    tracing::info!(
        pid,
        uffd_fd,
        regions = mappings.len(),
        total_bytes = total,
        "handshake complete",
    );
    let rt = Runtime::new(&memory_bin, mappings, uffd)?;
    let _stream_alive = stream;
    tracing::info!(pid, "starting fault loop");
    let result = rt.run();
    tracing::info!(pid, ?result, "fault loop returned");
    result
}
