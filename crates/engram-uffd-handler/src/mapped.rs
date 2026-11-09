//! Read-only shared mappings of chunk files, so an install copies
//! straight from the page cache or shared memory into the guest.

use std::fs::File;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::ptr::NonNull;

use bytes::Bytes;

/// A `PROT_READ`, `MAP_SHARED` mapping, unmapped on drop.
pub struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is process-wide and this type only reads it.
unsafe impl Send for Mapping {}
// SAFETY: as above; shared references only read.
unsafe impl Sync for Mapping {}

#[cfg(target_os = "linux")]
const MAP_POPULATE: libc::c_int = libc::MAP_POPULATE;
#[cfg(not(target_os = "linux"))]
const MAP_POPULATE: libc::c_int = 0;

impl Mapping {
    /// Map `len` bytes of `fd` and fault them in now, so the install
    /// takes no page faults on its source.
    pub fn populated(fd: BorrowedFd<'_>, len: usize) -> std::io::Result<Self> {
        Self::map(fd, len, MAP_POPULATE)
    }

    /// Map `len` bytes of `fd`; pages fault in on first read. The
    /// caller reads only bytes below the file's size.
    pub fn lazy(fd: BorrowedFd<'_>, len: usize) -> std::io::Result<Self> {
        Self::map(fd, len, 0)
    }

    fn map(fd: BorrowedFd<'_>, len: usize, flags: libc::c_int) -> std::io::Result<Self> {
        if len == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "cannot map zero bytes",
            ));
        }
        // SAFETY: a fresh mapping at a kernel-chosen address; no
        // existing memory is affected. The fd stays valid for the call.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED | flags,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let ptr =
            NonNull::new(ptr.cast::<u8>()).ok_or_else(|| std::io::Error::other("mmap null"))?;
        Ok(Self { ptr, len })
    }

    /// The first `len` bytes.
    ///
    /// # Safety
    /// `len` is at most the mapping's length and the file's size, and
    /// no process writes those bytes while the returned slice lives.
    pub unsafe fn prefix(&self, len: usize) -> &[u8] {
        debug_assert!(len <= self.len);
        // SAFETY: the caller's contract; the mapping is live for &self.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: ptr/len are exactly what mmap returned, unmapped once.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

/// A whole immutable file as a [`Mapping`].
struct WholeFile(Mapping);

impl AsRef<[u8]> for WholeFile {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: the mapping covers the whole file, and a chunk file is
        // never written or truncated after it is visible: the cache
        // writes a temp file and renames it, and evicts by unlink. An
        // unlink leaves this mapping valid.
        unsafe { self.0.prefix(self.0.len) }
    }
}

/// The bytes of an immutable chunk file, without a copy. The file may
/// be unlinked after this returns.
pub fn chunk_file_bytes(file: &File) -> std::io::Result<Bytes> {
    use std::os::fd::AsFd;
    let len = usize::try_from(file.metadata()?.len())
        .map_err(|_| std::io::Error::other("chunk file larger than the address space"))?;
    if len == 0 {
        return Ok(Bytes::new());
    }
    Ok(Bytes::from_owner(WholeFile(Mapping::populated(
        file.as_fd(),
        len,
    )?)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn chunk_file_bytes_survive_an_unlink() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chunk");
        let body: Vec<u8> = (0..20_000u32).map(|i| i as u8).collect();
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&body)
            .unwrap();
        let bytes = chunk_file_bytes(&File::open(&path).unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(bytes.as_ref(), body.as_slice());
    }

    #[test]
    fn an_empty_chunk_file_is_empty_bytes() {
        let file = tempfile::tempfile().unwrap();
        assert!(chunk_file_bytes(&file).unwrap().is_empty());
    }
}
