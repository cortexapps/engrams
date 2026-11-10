//! The first-touch record: the guest memory chunks a VM touched first.

use uuid::Uuid;

/// The chunks of guest memory a VM touched first, in order, by position.
/// A position survives the next checkpoint (a hash does not: the pages a
/// resume touches first are the pages the next checkpoint rewrites), so a
/// restore resolves these indices through its own manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FirstTouch {
    /// Chunk indices in first-touch order, each with the wall-clock time
    /// (Unix ms) of its first fault.
    pub entries: Vec<(u32, i64)>,
}

/// Bytes per [`FirstTouch`] entry: a little-endian `u32` index, then a
/// little-endian `i64` time.
const ENTRY_BYTES: usize = 12;

impl FirstTouch {
    /// The handler writes the record to this file in its report directory.
    pub const FILE: &'static str = "first-touch.bin";

    /// The store key of a session's published prefix: the indices only, as
    /// little-endian `u32`s ([`encode_indices`](Self::encode_indices)).
    pub fn key(session: Uuid) -> String {
        format!("traces/{session}/first-touch.v1")
    }

    /// The store key of the indices a session touched after its published
    /// prefix, in touch order, encoded as for [`Self::key`]. A build that
    /// does not know it reads only the prefix.
    pub fn rest_key(session: Uuid) -> String {
        format!("traces/{session}/first-touch-rest.v1")
    }

    /// The store key of the root disk chunks a session read before the
    /// copy of a hydrating restore reached them, in order, encoded as for
    /// [`Self::key`]. The next restore fetches the ones it must copy first.
    pub fn disk_key(session: Uuid) -> String {
        format!("traces/{session}/disk-first-touch.v1")
    }

    /// The indices touched up to and including `until_ms`, in order.
    pub fn until(&self, until_ms: i64) -> Vec<u32> {
        self.entries
            .iter()
            .take_while(|(_, at)| *at <= until_ms)
            .map(|(idx, _)| *idx)
            .collect()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.entries.len() * ENTRY_BYTES);
        for (idx, at) in &self.entries {
            out.extend_from_slice(&idx.to_le_bytes());
            out.extend_from_slice(&at.to_le_bytes());
        }
        out
    }

    /// `None` when `bytes` is not a whole number of entries.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if !bytes.len().is_multiple_of(ENTRY_BYTES) {
            return None;
        }
        let (entries, _) = bytes.as_chunks::<ENTRY_BYTES>();
        let entries = entries
            .iter()
            .map(|e| {
                let (idx, at) = e.split_at(4);
                (
                    u32::from_le_bytes(idx.try_into().expect("4 bytes")),
                    i64::from_le_bytes(at.try_into().expect("8 bytes")),
                )
            })
            .collect();
        Some(Self { entries })
    }

    pub fn encode_indices(indices: &[u32]) -> Vec<u8> {
        indices.iter().flat_map(|i| i.to_le_bytes()).collect()
    }

    /// `None` when `bytes` is not a whole number of indices.
    pub fn decode_indices(bytes: &[u8]) -> Option<Vec<u32>> {
        if !bytes.len().is_multiple_of(4) {
            return None;
        }
        let (indices, _) = bytes.as_chunks::<4>();
        Some(indices.iter().map(|i| u32::from_le_bytes(*i)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Published keys: other builds read these exact strings.
    #[test]
    fn first_touch_keys_are_stable() {
        let session = Uuid::from_u128(1);
        assert_eq!(
            FirstTouch::key(session),
            "traces/00000000-0000-0000-0000-000000000001/first-touch.v1"
        );
        assert_eq!(
            FirstTouch::rest_key(session),
            "traces/00000000-0000-0000-0000-000000000001/first-touch-rest.v1"
        );
    }

    #[test]
    fn first_touch_round_trips_and_rejects_a_torn_record() {
        let ft = FirstTouch {
            entries: vec![(7, 1_000), (u32::MAX, -1)],
        };
        let bytes = ft.encode();
        assert_eq!(bytes.len(), 24);
        assert_eq!(FirstTouch::decode(&bytes), Some(ft));
        assert_eq!(FirstTouch::decode(&bytes[..23]), None);
        let idx = FirstTouch::encode_indices(&[3, 1, 2]);
        assert_eq!(FirstTouch::decode_indices(&idx), Some(vec![3, 1, 2]));
        assert_eq!(FirstTouch::decode_indices(&idx[..5]), None);
    }
}
