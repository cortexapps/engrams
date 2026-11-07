//! Reused chunk buffers.
//!
//! A fresh 0.5–16 MiB allocation per chunk costs page faults on zeroed
//! pages each time the allocator has returned the memory to the kernel.
//! A [`ChunkBuf`] comes from a process-wide pool and goes back to it when
//! it drops, or, after [`ChunkBuf::freeze`], when the last [`Bytes`] that
//! shares it drops.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::LazyLock;

use bytes::Bytes;
use parking_lot::Mutex;

/// Only these lengths are pooled: the chunk sizes (powers of two). Any
/// other length (the short last chunk of a file) is allocated as usual.
const MIN_POOLED: usize = 64 * 1024;
const MAX_POOLED: usize = 16 * 1024 * 1024;

/// Idle bytes kept per length. Above it, a returned buffer is freed.
const IDLE_BYTES_PER_LEN: usize = 64 * 1024 * 1024;

static POOL: LazyLock<Mutex<HashMap<usize, Vec<Vec<u8>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn pooled(len: usize) -> bool {
    (MIN_POOLED..=MAX_POOLED).contains(&len) && len.is_power_of_two()
}

/// A buffer of a fixed length, returned to the pool on drop.
pub struct ChunkBuf {
    buf: Vec<u8>,
}

impl ChunkBuf {
    /// A buffer of `len` bytes. A reused buffer still holds the bytes of
    /// its last use: the caller overwrites all of it.
    pub fn take(len: usize) -> Self {
        let reused = if pooled(len) {
            POOL.lock().get_mut(&len).and_then(Vec::pop)
        } else {
            None
        };
        Self {
            buf: reused.unwrap_or_else(|| vec![0; len]),
        }
    }

    /// A buffer of `len` zero bytes.
    pub fn zeroed(len: usize) -> Self {
        let mut b = Self::take(len);
        b.fill(0);
        b
    }

    /// The buffer as `Bytes`, without a copy. It returns to the pool when
    /// the last clone drops.
    pub fn freeze(self) -> Bytes {
        Bytes::from_owner(self)
    }
}

impl Drop for ChunkBuf {
    fn drop(&mut self) {
        let len = self.buf.len();
        if !pooled(len) {
            return;
        }
        let buf = std::mem::take(&mut self.buf);
        let mut pool = POOL.lock();
        let idle = pool.entry(len).or_default();
        if (idle.len() + 1) * len <= IDLE_BYTES_PER_LEN {
            idle.push(buf);
        }
    }
}

impl Deref for ChunkBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.buf
    }
}

impl DerefMut for ChunkBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.buf
    }
}

impl AsRef<[u8]> for ChunkBuf {
    fn as_ref(&self) -> &[u8] {
        &self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frozen_buffer_returns_to_the_pool_when_its_last_clone_drops() {
        // A length no other test uses, so the pool slot is this test's.
        let len = 2 * 1024 * 1024;
        let mut b = ChunkBuf::take(len);
        b[0] = 7;
        let ptr = b.as_ptr();
        let bytes = b.freeze();
        let clone = bytes.clone();
        drop(bytes);
        assert_eq!(clone[0], 7);
        drop(clone);

        let again = ChunkBuf::take(len);
        assert_eq!(again.as_ptr(), ptr, "the same buffer is reused");
        assert_eq!(again.len(), len);
    }

    #[test]
    fn zeroed_clears_a_reused_buffer() {
        let len = 4 * 1024 * 1024;
        let mut b = ChunkBuf::take(len);
        b.fill(0xAB);
        drop(b);
        assert!(ChunkBuf::zeroed(len).iter().all(|&x| x == 0));
    }

    #[test]
    fn an_odd_length_is_not_pooled() {
        let b = ChunkBuf::take(100_000);
        assert_eq!(b.len(), 100_000);
        drop(b);
        assert!(!POOL.lock().contains_key(&100_000));
    }
}
