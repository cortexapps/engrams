//! ADR 0075: the READ-ONLY view of the chunk-cache directory.
//!
//! One process per host — the writer (host-agent today, ADR 0076's
//! substrated later) — owns population, pins, eviction, and the disk
//! budget through [`crate::cache::ChunkCache`]. Every other process
//! holds this type instead: a `ChunkCacheReader` has NO write, unlink,
//! pin, or sweep surface, so a client that compiles cannot mutate the
//! directory — the multi-writer race class (#437's `.partial` race,
//! the pin-invisible eviction hazard, dueling budget sweeps) is
//! unrepresentable at the type level, not discouraged by convention.
//!
//! Misses are NOT resolved here: the client requests population over
//! the substrate socket (`engram-substrate-proto`) and reads the fd
//! the writer returns, or — writer unreachable past its retry budget —
//! falls back to a direct blob-store fetch served from memory, never
//! written to this directory.
//!
//! Deliberately synchronous (`std::fs`): the primary caller is the
//! uffd fault loop, a blocking thread that must not grow a runtime.

use std::path::{Path, PathBuf};

use crate::ChunkHash;

/// Shared layout: `<root>/<hex[..2]>/<hex[2..]>`. ONE definition for
/// writer and reader — `ChunkCache::path_for` delegates here, so the
/// two views can never disagree on where a chunk lives.
pub fn chunk_path(root: &Path, hash: ChunkHash) -> PathBuf {
    let hex = hash.to_hex();
    root.join(&hex[..2]).join(&hex[2..])
}

/// Read-only accessor over a chunk-cache root.
#[derive(Clone, Debug)]
pub struct ChunkCacheReader {
    root: PathBuf,
}

impl ChunkCacheReader {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path_for(&self, hash: ChunkHash) -> PathBuf {
        chunk_path(&self.root, hash)
    }

    /// Read a resident chunk. `Ok(None)` = not resident (request
    /// population); errors are real I/O failures worth surfacing.
    pub fn read(&self, hash: ChunkHash) -> std::io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.path_for(hash)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn contains_on_disk(&self, hash: ChunkHash) -> bool {
        self.path_for(hash).try_exists().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(byte: u8) -> ChunkHash {
        ChunkHash::from_bytes([byte; 32])
    }

    #[test]
    fn read_hit_and_miss() {
        let dir = tempfile::tempdir().unwrap();
        let reader = ChunkCacheReader::new(dir.path());
        let h = hash_of(0xAB);
        assert!(reader.read(h).unwrap().is_none());
        assert!(!reader.contains_on_disk(h));

        let p = reader.path_for(h);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"bytes").unwrap();
        assert_eq!(reader.read(h).unwrap().unwrap(), b"bytes");
        assert!(reader.contains_on_disk(h));
    }
}
