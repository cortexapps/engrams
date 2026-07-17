//! Seeded entropy: the same seed replays the same id sequence.
//!
//! Hand-rolled SplitMix64 rather than a `rand` dependency — ADR 0098
//! reserves `rand_chacha` for the D5 scheduler's forked streams; the
//! substrate needs only a small, portable, stable generator. SplitMix64
//! is the canonical seeder (fixed constants, no state beyond one u64),
//! so its output is stable across platforms and releases of this crate.

use std::ops::Range;

use engram_core::traits::Entropy;
use parking_lot::Mutex;

#[derive(Debug)]
pub struct SimEntropy {
    state: Mutex<u64>,
}

impl SimEntropy {
    pub fn seeded(seed: u64) -> Self {
        Self {
            state: Mutex::new(seed),
        }
    }

    fn next_u64(&self) -> u64 {
        let mut s = self.state.lock();
        *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl Entropy for SimEntropy {
    fn uuid(&self) -> uuid::Uuid {
        let hi = self.next_u64();
        let lo = self.next_u64();
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&hi.to_be_bytes());
        bytes[8..].copy_from_slice(&lo.to_be_bytes());
        // Stamp RFC 4122 v4 bits so the ids are indistinguishable in
        // shape from OsEntropy's (round-trips through PG's UUID type,
        // version-sniffing code, etc).
        bytes[6] = (bytes[6] & 0x0F) | 0x40;
        bytes[8] = (bytes[8] & 0x3F) | 0x80;
        uuid::Uuid::from_bytes(bytes)
    }

    fn u64(&self, range: Range<u64>) -> u64 {
        assert!(!range.is_empty(), "empty entropy range");
        let span = range.end - range.start;
        range.start + (self.next_u64() % span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let a = SimEntropy::seeded(42);
        let b = SimEntropy::seeded(42);
        let ids_a: Vec<_> = (0..64).map(|_| a.uuid()).collect();
        let ids_b: Vec<_> = (0..64).map(|_| b.uuid()).collect();
        assert_eq!(ids_a, ids_b);
        // And a different seed diverges immediately.
        let c = SimEntropy::seeded(43);
        assert_ne!(ids_a[0], c.uuid());
    }

    #[test]
    fn uuids_are_v4_shaped_and_unique() {
        let e = SimEntropy::seeded(7);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..1000 {
            let id = e.uuid();
            assert_eq!(id.get_version_num(), 4);
            assert!(seen.insert(id), "collision in 1000 draws");
        }
    }

    #[test]
    fn ranged_u64_in_bounds() {
        let e = SimEntropy::seeded(9);
        for _ in 0..1000 {
            let v = e.u64(5..17);
            assert!((5..17).contains(&v));
        }
    }
}
