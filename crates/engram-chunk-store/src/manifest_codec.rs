//! The two byte encodings of a [`Manifest`]: JSON and a packed binary form.
//!
//! [`Manifest::decode`] reads both. A binary manifest starts with
//! [`BINARY_MAGIC`], which no JSON text can start with. All integers are
//! little-endian:
//!
//! ```text
//! magic             4   b"EGMF"
//! format_version    2   BINARY_FORMAT_VERSION
//! schema_version    4
//! kind              1   0 = disk, 1 = memory
//! chunk_size        8
//! total_bytes       8
//! flags             1   bit 0: parent, bit 1: working_set_trace,
//!                       bit 2: trace has a host_id
//! parent           24   manifest_id (16) + version (8), if bit 0
//! trace            16   manifest_id, if bit 1
//! trace host       16   host_id, if bit 2
//! annotations_len   4   0 = null
//! annotations       n   JSON text
//! chunk_count       8
//! chunks       40 * c   offset (8) + sha256 (32), in manifest order
//! ```
//!
//! A new field is a new `format_version`; a reader refuses a version it
//! does not know. Trailing bytes are an error.

use uuid::Uuid;

use crate::error::{ChunkStoreError, Result};
use crate::manifest::{ChunkHash, ChunkRef, ChunkSize, Manifest, ManifestKind, ManifestRef};
use crate::working_set::TraceRef;

/// First bytes of a binary manifest.
pub const BINARY_MAGIC: [u8; 4] = *b"EGMF";

/// The binary layout [`Manifest::encode_binary`] writes.
pub const BINARY_FORMAT_VERSION: u16 = 1;

const FLAG_PARENT: u8 = 1 << 0;
const FLAG_TRACE: u8 = 1 << 1;
const FLAG_TRACE_HOST: u8 = 1 << 2;
const CHUNK_ENTRY_LEN: usize = 8 + 32;

impl Manifest {
    /// Decode a manifest from either encoding. Does not call
    /// [`Manifest::validate`].
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.starts_with(&BINARY_MAGIC) {
            decode_binary(&bytes[BINARY_MAGIC.len()..])
        } else {
            let mut de = serde_json::Deserializer::from_slice(bytes);
            let m = JsonManifest::deserialize(&mut de)?;
            de.end()?;
            Ok(m)
        }
    }

    /// The JSON encoding.
    pub fn encode_json(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// The binary encoding.
    pub fn encode_binary(&self) -> Result<Vec<u8>> {
        let annotations = match &self.annotations {
            serde_json::Value::Null => Vec::new(),
            value => serde_json::to_vec(value)?,
        };
        let annotations_len = u32::try_from(annotations.len())
            .map_err(|_| ChunkStoreError::MalformedManifest("annotations exceed 4 GiB".into()))?;
        let mut out =
            Vec::with_capacity(80 + annotations.len() + self.chunks.len() * CHUNK_ENTRY_LEN);
        out.extend_from_slice(&BINARY_MAGIC);
        out.extend_from_slice(&BINARY_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.schema_version.to_le_bytes());
        out.push(match self.kind {
            ManifestKind::Disk => 0,
            ManifestKind::Memory => 1,
        });
        out.extend_from_slice(&self.chunk_size.as_u64().to_le_bytes());
        out.extend_from_slice(&self.total_bytes.to_le_bytes());
        let mut flags = 0;
        if self.parent.is_some() {
            flags |= FLAG_PARENT;
        }
        if let Some(trace) = &self.working_set_trace {
            flags |= FLAG_TRACE;
            if trace.host_id.is_some() {
                flags |= FLAG_TRACE_HOST;
            }
        }
        out.push(flags);
        if let Some(parent) = &self.parent {
            out.extend_from_slice(parent.manifest_id.as_bytes());
            out.extend_from_slice(&parent.version.to_le_bytes());
        }
        if let Some(trace) = &self.working_set_trace {
            out.extend_from_slice(trace.manifest_id.as_bytes());
            if let Some(host) = &trace.host_id {
                out.extend_from_slice(host.as_bytes());
            }
        }
        out.extend_from_slice(&annotations_len.to_le_bytes());
        out.extend_from_slice(&annotations);
        out.extend_from_slice(&(self.chunks.len() as u64).to_le_bytes());
        for chunk in &self.chunks {
            out.extend_from_slice(&chunk.offset.to_le_bytes());
            out.extend_from_slice(chunk.hash.as_bytes());
        }
        Ok(out)
    }
}

/// The JSON reader. `Manifest` has no `Deserialize` of its own, so every
/// reader goes through [`Manifest::decode`] and accepts both encodings.
#[derive(serde::Deserialize)]
#[serde(remote = "Manifest")]
struct JsonManifest {
    schema_version: u32,
    kind: ManifestKind,
    chunk_size: ChunkSize,
    total_bytes: u64,
    chunks: Vec<ChunkRef>,
    #[serde(default)]
    parent: Option<ManifestRef>,
    #[serde(default)]
    working_set_trace: Option<TraceRef>,
    #[serde(default)]
    annotations: serde_json::Value,
}

fn malformed(what: &str) -> ChunkStoreError {
    ChunkStoreError::MalformedManifest(format!("binary manifest: {what}"))
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8]> {
        if self.0.len() < n {
            return Err(malformed(&format!("truncated at {what}")));
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self, what: &str) -> Result<[u8; N]> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N, what)?);
        Ok(out)
    }

    fn u8(&mut self, what: &str) -> Result<u8> {
        Ok(self.array::<1>(what)?[0])
    }

    fn u16(&mut self, what: &str) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array(what)?))
    }

    fn u32(&mut self, what: &str) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array(what)?))
    }

    fn u64(&mut self, what: &str) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array(what)?))
    }

    fn uuid(&mut self, what: &str) -> Result<Uuid> {
        Ok(Uuid::from_bytes(self.array(what)?))
    }
}

fn decode_binary(body: &[u8]) -> Result<Manifest> {
    let mut r = Reader(body);
    let format_version = r.u16("format_version")?;
    if format_version != BINARY_FORMAT_VERSION {
        return Err(malformed(&format!(
            "format version {format_version} is not {BINARY_FORMAT_VERSION}"
        )));
    }
    let schema_version = r.u32("schema_version")?;
    let kind = match r.u8("kind")? {
        0 => ManifestKind::Disk,
        1 => ManifestKind::Memory,
        other => return Err(malformed(&format!("unknown kind {other}"))),
    };
    let chunk_size = ChunkSize::bytes(r.u64("chunk_size")?);
    let total_bytes = r.u64("total_bytes")?;
    let flags = r.u8("flags")?;
    if flags & !(FLAG_PARENT | FLAG_TRACE | FLAG_TRACE_HOST) != 0
        || (flags & FLAG_TRACE_HOST != 0 && flags & FLAG_TRACE == 0)
    {
        return Err(malformed(&format!("invalid flags {flags:#04x}")));
    }
    let parent = if flags & FLAG_PARENT != 0 {
        Some(ManifestRef {
            manifest_id: r.uuid("parent id")?,
            version: r.u64("parent version")?,
        })
    } else {
        None
    };
    let working_set_trace = if flags & FLAG_TRACE != 0 {
        let manifest_id = r.uuid("trace id")?;
        let host_id = if flags & FLAG_TRACE_HOST != 0 {
            Some(r.uuid("trace host")?)
        } else {
            None
        };
        Some(TraceRef {
            manifest_id,
            host_id,
        })
    } else {
        None
    };
    let annotations_len = r.u32("annotations_len")? as usize;
    let annotations = match r.take(annotations_len, "annotations")? {
        [] => serde_json::Value::Null,
        text => serde_json::from_slice(text)?,
    };
    let chunk_count = r.u64("chunk_count")?;
    let entries = r.0;
    if usize::try_from(chunk_count)
        .ok()
        .and_then(|c| c.checked_mul(CHUNK_ENTRY_LEN))
        != Some(entries.len())
    {
        return Err(malformed(&format!(
            "{chunk_count} chunks do not fill the {} remaining bytes",
            entries.len()
        )));
    }
    let chunks = entries
        .as_chunks::<CHUNK_ENTRY_LEN>()
        .0
        .iter()
        .map(|entry| {
            let (offset, hash) = entry.split_at(8);
            ChunkRef {
                offset: u64::from_le_bytes(offset.try_into().expect("8-byte split")),
                hash: ChunkHash::from_bytes(hash.try_into().expect("32-byte split")),
            }
        })
        .collect();
    Ok(Manifest {
        schema_version,
        kind,
        chunk_size,
        total_bytes,
        chunks,
        parent,
        working_set_trace,
        annotations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        let mut m = Manifest::empty(ManifestKind::Memory, 4 * 1024 * 1024);
        m.chunks = (0..8)
            .map(|i| ChunkRef {
                offset: i * 512 * 1024,
                hash: ChunkHash::of(&[i as u8]),
            })
            .collect();
        m.parent = Some(ManifestRef {
            manifest_id: Uuid::from_u128(7),
            version: 3,
        });
        m.working_set_trace = Some(TraceRef::host(Uuid::from_u128(8), Uuid::from_u128(9)));
        m.annotations = serde_json::json!({ "image_tag": "warm-1" });
        m
    }

    #[test]
    fn binary_round_trip_is_lossless() {
        let m = sample();
        assert_eq!(Manifest::decode(&m.encode_binary().unwrap()).unwrap(), m);
        let bare = Manifest::empty(ManifestKind::Disk, 0);
        assert_eq!(
            Manifest::decode(&bare.encode_binary().unwrap()).unwrap(),
            bare
        );
    }

    #[test]
    fn json_still_decodes() {
        let m = sample();
        assert_eq!(Manifest::decode(&m.encode_json().unwrap()).unwrap(), m);
    }

    /// The JSON form the previous release wrote, as literal bytes.
    #[test]
    fn json_from_the_previous_release_decodes() {
        let json = br#"{"schema_version":1,"kind":{"type":"memory"},"chunk_size":524288,"total_bytes":1048576,"chunks":[{"offset":0,"hash":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"}],"parent":{"manifest_id":"00000000-0000-0000-0000-000000000007","version":3},"working_set_trace":{"manifest_id":"00000000-0000-0000-0000-000000000008"},"annotations":{"image_tag":"warm-1"}}"#;
        let m = Manifest::decode(json).unwrap();
        assert_eq!(m.kind, ManifestKind::Memory);
        assert_eq!(
            m.chunks,
            vec![ChunkRef {
                offset: 0,
                hash: ChunkHash::of(b"abc"),
            }]
        );
        assert_eq!(m.parent.unwrap().version, 3);
        assert_eq!(
            m.working_set_trace,
            Some(TraceRef::canonical(Uuid::from_u128(8)))
        );
        assert_eq!(m.annotations["image_tag"], "warm-1");
        assert_eq!(Manifest::decode(&m.encode_binary().unwrap()).unwrap(), m);
    }

    #[test]
    fn every_truncation_of_a_binary_manifest_is_an_error() {
        let bytes = sample().encode_binary().unwrap();
        for len in 0..bytes.len() {
            assert!(Manifest::decode(&bytes[..len]).is_err(), "prefix {len}");
        }
        let mut long = bytes.clone();
        long.push(0);
        assert!(Manifest::decode(&long).is_err());
    }

    #[test]
    fn unknown_format_version_is_refused() {
        let mut bytes = sample().encode_binary().unwrap();
        bytes[4] = 2;
        assert!(matches!(
            Manifest::decode(&bytes),
            Err(ChunkStoreError::MalformedManifest(_))
        ));
    }
}
