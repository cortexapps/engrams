//! Wire protocol for the ADR 0045 C2 post-copy page channel.
//!
//! Two conversations share the same framing (`4-byte big-endian length` +
//! bincode body, the harness-proto/agentd precedent):
//!
//! 1. **dest uffd-handler ↔ source host-agent** over TCP (the source's
//!    dedicated page-server listener, default port 9102, one listener per
//!    host with `export_id` routing). The handler is the deliberately
//!    minimal client — sync `std::io` framing only, no tonic, no tokio —
//!    because its fault loop is a blocking thread and every dependency it
//!    grows is attack/bloat surface inside the page-fault path.
//! 2. **handler → host-agent** over the `--control-sock` UDS: one-way
//!    [`HandlerControl`] progress/failure reports.
//!
//! ## Conversation shape (page channel)
//!
//! ```text
//!   handler ──[ ToSource::Hello { version, export_id, token } ]──► source
//!   source  ──[ FromSource::HelloAck { version, chunk_size, total_bytes } ]──► handler
//!   source  ──[ FromSource::Seal { bitmap } ]──► handler        (pushed once, post-scan)
//!   handler ──[ ToSource::NeedAt { req_id, chunk_offset } ]──► source
//!   source  ──[ FromSource::Page | ZeroChunk | AltSource ]──► handler
//!   ...
//!   handler ──[ ToSource::DrainDone { .. } ]──► source
//!   <connection closed by the handler>
//! ```
//!
//! A handler may open MULTIPLE connections per export (one for the fault
//! path, one or more for the background drain) — the token is per-export,
//! not per-connection, and every authenticated connection receives the
//! `Seal` push. `GET_STATE` from the original ADR sketch is deliberately
//! ABSENT: `state.bin` rides the C1 gRPC `MigrationFetch` (`StateBin`
//! item) between host-agents; the handler never needs it.
//!
//! ## Soundness notes encoded in the message set
//!
//! - `Page.hash` (blake3) lets the dest verify peer bytes before `UFFDIO_COPY`;
//!   a mismatch is fatal (peer-authoritative content has no second
//!   source).
//! - `AltSource` demotes an over-approximated dirty chunk to class 2: the
//!   source proved (by hashing its live bytes) that the chunk equals the
//!   durable manifest entry, so the dest may fetch it from cache/GCS via
//!   its normal `fetch_chunk` path.
//! - `ZeroChunk` keeps an idle guest's freed memory off the wire: a
//!   ZEROPAGE-installed or zeroed-COW chunk classifies dirty in the
//!   pagemap scan, but shipping 512 KiB of zeros per chunk would dominate
//!   the drain.

use std::io::{Read, Write};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Protocol version carried in `Hello`/`HelloAck`. Bump on any
/// wire-incompatible change; the server rejects mismatches loudly.
///
/// v2: `Page.lz4` — payloads ship lz4-block-compressed when that is
/// smaller, and `sha256` covers the WIRE bytes. The measured per-fault
/// cost on the no-SHA-NI fleet was ~6.7 ms, dominated by the 512 KiB
/// transfer + double sha256 — compression cuts all three.
///
/// v3: `Hello.purpose` — connections identify as Fault or Drain. The
/// source skips the AltSource classify hash on fault connections: a
/// demote there ADDS a dest-side cache/GCS fetch (strictly worse
/// latency than shipping the resident bytes), and the classify sha256
/// of the raw 512 KiB was ~1 ms of the per-fault constant on the
/// no-SHA-NI fleet. Drain connections keep the demote (it saves wire
/// and the drain is latency-insensitive).
///
/// v4: `Page.hash` is blake3 (was sha256) — measured 1.63 ms/serve in
/// the encode leg, ~1.2 ms of it soft sha256 of the compressed wire
/// bytes (no SHA-NI on the fleet); blake3 is cryptographic at
/// ~1–2 GB/s in software. `AltSource.durable_sha256` STAYS sha256 —
/// that is the chunk-store's content address, not wire integrity.
pub const PROTO_VERSION: u32 = 4;

/// Wire-integrity hash for `Page` payloads (v4: blake3 over the wire
/// bytes — the SAME bytes shipped, compressed or raw). One helper so
/// both endpoints can never disagree on the algorithm.
pub fn wire_hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// What a peer connection is FOR — the source's serve policy keys on
/// it (see the v3 note on [`PROTO_VERSION`]).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConnPurpose {
    /// The handler's fault-loop connection: latency-critical, single
    /// in-flight request, a stalled vCPU behind every frame.
    Fault,
    /// Background drain: throughput-oriented, pipelined, yields to
    /// faults on the dest side.
    Drain,
}

/// Default TCP port for the source host-agent's page-server listener.
pub const DEFAULT_PEER_PORT: u16 = 9102;

/// Maximum size of one framed message (16 MiB — a 512 KiB chunk plus
/// envelope fits with room; same cap discipline as the agentd proto).
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Per-chunk dirty bitmap sealed by the source's post-pause pagemap scan.
/// Bit `i` set ⇒ chunk `i` is peer-authoritative (dirtied since the last
/// checkpoint, or over-approximated — `AltSource` demotes the latter on
/// request). At the substrate's 512 KiB memory chunks this is 2–4 KiB for
/// 8–16 GiB guests, so a plain bitvec beats any compressed set.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealBitmap {
    /// Chunk size in bytes (must equal the session manifest's).
    pub chunk_size: u64,
    /// Total number of chunks covered (== ceil(total_bytes / chunk_size)).
    pub chunk_count: u64,
    /// LSB-first packed bits, `ceil(chunk_count / 8)` bytes; trailing bits
    /// in the last byte MUST be zero (checked by [`SealBitmap::validate`]).
    pub bits: Vec<u8>,
}

impl SealBitmap {
    /// All-clear bitmap for `chunk_count` chunks.
    pub fn new(chunk_size: u64, chunk_count: u64) -> Self {
        Self {
            chunk_size,
            chunk_count,
            bits: vec![0u8; chunk_count.div_ceil(8) as usize],
        }
    }

    /// Is chunk `idx` sealed? Out-of-range reads are `false` (callers
    /// branch on this in the fault path; a panic there wedges a vCPU).
    pub fn get(&self, idx: u64) -> bool {
        if idx >= self.chunk_count {
            return false;
        }
        let byte = (idx / 8) as usize;
        let bit = (idx % 8) as u8;
        self.bits.get(byte).is_some_and(|b| b & (1 << bit) != 0)
    }

    /// Seal chunk `idx`. Panics on out-of-range (builder-side only).
    pub fn set(&mut self, idx: u64) {
        assert!(idx < self.chunk_count, "SealBitmap::set out of range");
        let byte = (idx / 8) as usize;
        let bit = (idx % 8) as u8;
        self.bits[byte] |= 1 << bit;
    }

    /// Number of sealed chunks.
    pub fn count_ones(&self) -> u64 {
        self.bits.iter().map(|b| b.count_ones() as u64).sum()
    }

    /// Structural validity: byte length matches `chunk_count` and the
    /// trailing bits of the last byte are zero. Run on every received
    /// bitmap before trusting it in the fault path.
    pub fn validate(&self) -> Result<(), String> {
        let want = self.chunk_count.div_ceil(8) as usize;
        if self.bits.len() != want {
            return Err(format!(
                "bitmap byte length {} != expected {} for {} chunks",
                self.bits.len(),
                want,
                self.chunk_count
            ));
        }
        let tail_bits = (self.chunk_count % 8) as u8;
        if tail_bits != 0 {
            let mask = !((1u16 << tail_bits) - 1) as u8;
            if self.bits.last().is_some_and(|last| last & mask != 0) {
                return Err("trailing bits past chunk_count are set".into());
            }
        }
        Ok(())
    }
}

/// dest uffd-handler → source host-agent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToSource {
    /// First frame on every connection. The server validates `version`,
    /// resolves the export by `export_id`, and constant-time-compares
    /// `token` against the per-export secret minted at capture.
    Hello {
        version: u32,
        export_id: String,
        token: String,
        purpose: ConnPurpose,
    },
    /// Demand-fault or drain request for the sealed chunk containing
    /// `chunk_offset` (a chunk-aligned byte offset in the snapshot memory
    /// space). The server rejects unsealed or unaligned offsets — those
    /// are protocol bugs, not races.
    NeedAt { req_id: u64, chunk_offset: u64 },
    /// Class-2 by-hash pull from the source's NVMe cache (allowlist-
    /// gated, same posture as the gRPC `MigrationFetch` chunk items).
    /// The fallback when the dest's local fetch misses mid-drain.
    GetChunk { req_id: u64, hash: [u8; 32] },
    /// The drain finished: every sealed chunk is installed (or demoted
    /// and durably fetchable). The source may release the export early.
    /// Stats are informational (metrics/logs).
    DrainDone {
        pulled: u64,
        alt_sourced: u64,
        zero_chunks: u64,
    },
}

/// source host-agent → dest uffd-handler.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum FromSource {
    /// Accepts the `Hello`. `chunk_size`/`total_bytes` let the handler
    /// cross-check its own manifest geometry before serving any fault —
    /// a mismatch is fatal-loud (manifest skew breaks `AltSource`
    /// soundness).
    HelloAck {
        version: u32,
        chunk_size: u64,
        total_bytes: u64,
    },
    /// Pushed (unsolicited) on every authenticated connection once the
    /// pagemap scan completes. Idempotent across connections; the
    /// handler keeps the first and sanity-checks duplicates.
    Seal { bitmap: SealBitmap },
    /// Peer-authoritative chunk bytes read via `process_vm_readv` from
    /// the paused source VM. `hash` is [`wire_hash`] over `bytes`
    /// (as-shipped); the handler MUST verify before installing.
    Page {
        req_id: u64,
        chunk_offset: u64,
        /// WIRE bytes: lz4-block-compressed (size-prepended) when
        /// `lz4`, raw otherwise (the source ships whichever is
        /// smaller — incompressible chunks go raw).
        bytes: Vec<u8>,
        /// [`wire_hash`] over the WIRE bytes — verify BEFORE
        /// decompressing.
        hash: [u8; 32],
        lz4: bool,
    },
    /// The whole chunk is zero bytes — install via the zero path instead
    /// of shipping 512 KiB of zeros.
    ZeroChunk { req_id: u64, chunk_offset: u64 },
    /// Over-approximation demote: the chunk's live content hashes equal
    /// to the durable manifest entry at this offset, so the dest can
    /// fetch it through its normal class-2 path (cache/GCS race).
    AltSource {
        req_id: u64,
        chunk_offset: u64,
        durable_sha256: [u8; 32],
    },
    /// `GetChunk` reply; the integrity check is the requested hash
    /// itself (content-addressed).
    ChunkBytes { req_id: u64, bytes: Vec<u8> },
    /// Protocol or serving error. `req_id: None` ⇒ connection-fatal
    /// (bad hello, version skew); `Some` ⇒ that request failed.
    Error {
        req_id: Option<u64>,
        message: String,
    },
}

/// Compress a page payload for the wire: returns the smaller of the
/// lz4 block (size-prepended) and the raw bytes, plus the `lz4` flag.
pub fn compress_page(raw: Vec<u8>) -> (Vec<u8>, bool) {
    let compressed = lz4_flex::block::compress_prepend_size(&raw);
    if compressed.len() < raw.len() {
        (compressed, true)
    } else {
        (raw, false)
    }
}

/// Reverse [`compress_page`] AFTER wire-integrity verification.
pub fn decompress_page(wire: Vec<u8>, lz4: bool) -> Result<Vec<u8>, String> {
    if !lz4 {
        return Ok(wire);
    }
    lz4_flex::block::decompress_size_prepended(&wire).map_err(|e| format!("lz4 decompress: {e}"))
}

/// handler → host-agent over the `--control-sock` UDS (same framing).
/// Strictly one-way; the handler never blocks on control backpressure
/// (the fault path has priority over observability).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum HandlerControl {
    /// The `Seal` arrived and faults can be served. The host-agent
    /// orders the FC snapshot load AFTER this (the handler also gates
    /// internally — soundness never depends on external ordering).
    Sealed {
        dirty_chunks: u64,
        total_chunks: u64,
        at_unix_ms: i64,
    },
    /// Periodic drain progress (every N chunks).
    DrainProgress { pulled: u64, remaining: u64 },
    /// All sealed chunks installed or demoted; the peer connections are
    /// closed and the handler is byte-identical to a C1 restore from
    /// here on. `faults`/`fault_us` are the FAULT-path totals (guest
    /// faults that round-tripped the peer + their cumulative wall) —
    /// the serial P2P cost inside the FC load + early execution, the
    /// restore-tail attribution the drain numbers can't show. Control
    /// sock is same-host (handler ↔ its own host-agent, one image), so
    /// extending the variant is bincode-safe.
    DrainDone {
        pulled: u64,
        alt_sourced: u64,
        zero_chunks: u64,
        ms: u64,
        faults: u64,
        fault_us: u64,
        fault_max_us: u64,
    },
    /// The peer is gone (dial/reconnect exhausted, frame error, or sha
    /// mismatch) with sealed chunks still uninstalled. FATAL by design:
    /// dirtied-since-checkpoint content has no sound second source. The
    /// host-agent pauses the dest VM and reports for the rung-1 rewind.
    PeerLost { remaining: u64, detail: String },
}

/// Read one length-prefixed frame off `r` and bincode-decode it.
/// Blocking; the handler's fault/drain threads own their sockets.
pub fn read_frame<R, T>(r: &mut R) -> std::io::Result<T>
where
    R: Read,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds MAX_FRAME_BYTES ({MAX_FRAME_BYTES})"),
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    bincode::deserialize(&body)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bincode: {e}")))
}

/// Bincode-encode `msg` and write it as a length-prefixed frame.
pub fn write_frame<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: Write,
    T: Serialize,
{
    let body = bincode::serialize(msg).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bincode: {e}"))
    })?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "encoded frame {} exceeds MAX_FRAME_BYTES ({MAX_FRAME_BYTES})",
                body.len()
            ),
        ));
    }
    let len = (body.len() as u32).to_be_bytes();
    w.write_all(&len)?;
    w.write_all(&body)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(msg: &T) {
        let mut buf = Vec::new();
        write_frame(&mut buf, msg).unwrap();
        let got: T = read_frame(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(&got, msg);
    }

    #[test]
    fn every_to_source_variant_round_trips() {
        round_trip(&ToSource::Hello {
            version: PROTO_VERSION,
            export_id: "ab12".into(),
            token: "secret".into(),
            purpose: ConnPurpose::Fault,
        });
        round_trip(&ToSource::Hello {
            version: PROTO_VERSION,
            export_id: "ab12".into(),
            token: "secret".into(),
            purpose: ConnPurpose::Drain,
        });
        round_trip(&ToSource::NeedAt {
            req_id: 7,
            chunk_offset: 512 * 1024 * 3,
        });
        round_trip(&ToSource::GetChunk {
            req_id: 8,
            hash: [0xAB; 32],
        });
        round_trip(&ToSource::DrainDone {
            pulled: 100,
            alt_sourced: 3,
            zero_chunks: 42,
        });
    }

    #[test]
    fn every_from_source_variant_round_trips() {
        round_trip(&FromSource::HelloAck {
            version: PROTO_VERSION,
            chunk_size: 512 * 1024,
            total_bytes: 128 * 1024 * 1024,
        });
        let mut bitmap = SealBitmap::new(512 * 1024, 17);
        bitmap.set(0);
        bitmap.set(16);
        round_trip(&FromSource::Seal { bitmap });
        round_trip(&FromSource::Page {
            req_id: 1,
            chunk_offset: 0,
            bytes: vec![0xCD; 512 * 1024],
            hash: [0x11; 32],
            lz4: false,
        });
        // The v2 compression helpers: a compressible chunk round-trips
        // through lz4; an incompressible one ships raw.
        let raw = vec![0xCD; 512 * 1024];
        let (wire, lz4) = compress_page(raw.clone());
        assert!(
            lz4 && wire.len() < raw.len(),
            "repetitive chunk must compress"
        );
        assert_eq!(decompress_page(wire, lz4).unwrap(), raw);
        let noise: Vec<u8> = (0..4096u32)
            .flat_map(|i| i.wrapping_mul(2654435761).to_le_bytes())
            .collect();
        let (wire, lz4) = compress_page(noise.clone());
        assert!(!lz4, "incompressible bytes must ship raw");
        assert_eq!(decompress_page(wire, lz4).unwrap(), noise);
        round_trip(&FromSource::ZeroChunk {
            req_id: 2,
            chunk_offset: 512 * 1024,
        });
        round_trip(&FromSource::AltSource {
            req_id: 3,
            chunk_offset: 1024 * 1024,
            durable_sha256: [0x22; 32],
        });
        round_trip(&FromSource::ChunkBytes {
            req_id: 4,
            bytes: vec![1, 2, 3],
        });
        round_trip(&FromSource::Error {
            req_id: None,
            message: "bad hello".into(),
        });
    }

    #[test]
    fn every_handler_control_variant_round_trips() {
        round_trip(&HandlerControl::Sealed {
            dirty_chunks: 12,
            total_chunks: 256,
            at_unix_ms: 1_770_000_000_000,
        });
        round_trip(&HandlerControl::DrainProgress {
            pulled: 6,
            remaining: 6,
        });
        round_trip(&HandlerControl::DrainDone {
            pulled: 10,
            alt_sourced: 1,
            zero_chunks: 1,
            ms: 1234,
            faults: 42,
            fault_us: 55_000,
            fault_max_us: 9_000,
        });
        round_trip(&HandlerControl::PeerLost {
            remaining: 3,
            detail: "connection reset".into(),
        });
    }

    #[test]
    fn oversize_frame_is_rejected_on_read() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(((MAX_FRAME_BYTES + 1) as u32).to_be_bytes()));
        buf.extend_from_slice(&[0u8; 16]);
        let err = read_frame::<_, ToSource>(&mut Cursor::new(&buf)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn truncated_frame_errors_cleanly() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &ToSource::NeedAt {
                req_id: 1,
                chunk_offset: 0,
            },
        )
        .unwrap();
        buf.truncate(buf.len() - 2);
        let err = read_frame::<_, ToSource>(&mut Cursor::new(&buf)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn seal_bitmap_edges() {
        // Zero chunks: empty bits, everything reads unsealed.
        let empty = SealBitmap::new(512 * 1024, 0);
        empty.validate().unwrap();
        assert_eq!(empty.count_ones(), 0);
        assert!(!empty.get(0));

        // Non-multiple-of-8 count: tail handling.
        let mut b = SealBitmap::new(512 * 1024, 13);
        b.validate().unwrap();
        b.set(12);
        b.validate().unwrap();
        assert!(b.get(12));
        assert!(!b.get(11));
        // Out-of-range read is false, not a panic.
        assert!(!b.get(13));
        assert!(!b.get(u64::MAX));
        assert_eq!(b.count_ones(), 1);

        // Trailing garbage bits past chunk_count fail validation.
        let mut bad = SealBitmap::new(512 * 1024, 13);
        *bad.bits.last_mut().unwrap() |= 1 << 7; // bit 15 of a 13-chunk map
        assert!(bad.validate().is_err());

        // Wrong byte length fails validation.
        let mut short = SealBitmap::new(512 * 1024, 64);
        short.bits.pop();
        assert!(short.validate().is_err());
    }

    #[test]
    fn set_panics_out_of_range() {
        let mut b = SealBitmap::new(512 * 1024, 8);
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| b.set(8)));
        assert!(res.is_err());
    }
}
