//! How long an eager restore of a disk writes before the guest boots
//! (ADR 0127 Decision 1). The estimate reads manifests only: the chunks
//! the eager path writes, split by whether the host holds each one
//! locally, at a rate for each kind.

use engram_chunk_store::{ChunkHash, ChunkStoreError, Manifest};

/// Write rates of an eager restore, in bytes per second.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rates {
    /// A chunk the host holds locally.
    pub local: f64,
    /// A chunk fetched from the chunk store.
    pub remote: f64,
}

/// The bytes an eager restore writes and the time it takes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Estimate {
    pub local_bytes: u64,
    pub remote_bytes: u64,
    pub seconds: f64,
}

impl Estimate {
    /// The eager restore does not finish within `budget_seconds`.
    pub fn exceeds(&self, budget_seconds: f64) -> bool {
        self.seconds > budget_seconds
    }
}

/// The estimate of writing `wanted` onto a volume that holds `base`, or
/// onto an empty volume. `is_local` says whether the host holds a chunk.
pub fn estimate(
    wanted: &Manifest,
    base: Option<&Manifest>,
    is_local: impl Fn(ChunkHash) -> bool,
    rates: Rates,
) -> Result<Estimate, ChunkStoreError> {
    let writes = match base {
        Some(base) => wanted.delta_from(base)?.write,
        None => wanted.chunks.clone(),
    };
    let chunk = wanted.chunk_size.as_u64();
    let (mut local_bytes, mut remote_bytes) = (0, 0);
    for c in writes {
        let bytes = chunk.min(wanted.total_bytes.saturating_sub(c.offset));
        if is_local(c.hash) {
            local_bytes += bytes;
        } else {
            remote_bytes += bytes;
        }
    }
    Ok(Estimate {
        local_bytes,
        remote_bytes,
        seconds: local_bytes as f64 / rates.local + remote_bytes as f64 / rates.remote,
    })
}

#[cfg(test)]
mod tests {
    use engram_chunk_store::{ChunkRef, ChunkSize, ManifestKind};

    use super::*;

    const CHUNK: u64 = 1000;
    const RATES: Rates = Rates {
        local: 4000.0,
        remote: 1000.0,
    };

    fn hash(byte: u8) -> ChunkHash {
        ChunkHash::from_bytes([byte; 32])
    }

    /// A manifest of `total` bytes whose chunk `i` has hash `pattern[i]`;
    /// 0 is a hole.
    fn manifest(total: u64, pattern: &[u8]) -> Manifest {
        Manifest {
            chunk_size: ChunkSize::bytes(CHUNK),
            chunks: pattern
                .iter()
                .enumerate()
                .filter(|(_, b)| **b != 0)
                .map(|(i, b)| ChunkRef {
                    offset: i as u64 * CHUNK,
                    hash: hash(*b),
                })
                .collect(),
            ..Manifest::empty(ManifestKind::Disk, total)
        }
    }

    #[test]
    fn with_no_base_every_chunk_is_written_and_a_short_last_chunk_counts_its_bytes() {
        let wanted = manifest(3500, &[1, 0, 3, 4]);
        let e = estimate(&wanted, None, |_| false, RATES).unwrap();
        assert_eq!((e.local_bytes, e.remote_bytes), (0, 2500));
        assert_eq!(e.seconds, 2.5);
    }

    #[test]
    fn over_a_base_only_the_chunks_that_differ_are_written() {
        let base = manifest(4000, &[1, 2, 3, 4]);
        let wanted = manifest(4000, &[1, 9, 3, 0]);
        let e = estimate(&wanted, Some(&base), |_| false, RATES).unwrap();
        assert_eq!((e.local_bytes, e.remote_bytes), (0, 1000));
        assert_eq!(e.seconds, 1.0);
        let other_shape = manifest(5000, &[1]);
        assert!(estimate(&wanted, Some(&other_shape), |_| false, RATES).is_err());
    }

    #[test]
    fn local_chunks_cost_the_local_rate() {
        let wanted = manifest(4000, &[1, 2, 3, 4]);
        let e = estimate(&wanted, None, |_| true, RATES).unwrap();
        assert_eq!((e.local_bytes, e.remote_bytes), (4000, 0));
        assert_eq!(e.seconds, 1.0);
        let e = estimate(&wanted, None, |h| h == hash(1), RATES).unwrap();
        assert_eq!((e.local_bytes, e.remote_bytes), (1000, 3000));
        assert_eq!(e.seconds, 3.25);
    }

    #[test]
    fn an_estimate_equal_to_the_budget_stays_eager() {
        let e = estimate(&manifest(2000, &[1, 2]), None, |_| false, RATES).unwrap();
        assert!(!e.exceeds(2.0));
        assert!(e.exceeds(1.999));
        let nothing = estimate(&manifest(2000, &[]), None, |_| false, RATES).unwrap();
        assert_eq!(nothing.seconds, 0.0);
        assert!(!nothing.exceeds(0.0));
    }
}
