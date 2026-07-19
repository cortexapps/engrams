//! A content-hash envelope for the host-local durable JSON formats (ADR 0098
//! Phase 3, R5 — the storage-fault model / "storage lies").
//!
//! `durable_record` (the `EvictionFinalizeRecord` / `CheckpointRecord` /
//! `CaptureJobRecord` / `ChainHeadRecord` engine) and the shutdown-spool
//! completeness marker are plain JSON. A byte-flip / bit-rot / lying fsync
//! that leaves the JSON *syntactically valid* — a version bumped, a manifest
//! ref altered, a stage advanced — is TRUSTED at load: the record parses, and
//! a resume drives it as durable truth. That is the silent-corruption class
//! this wave exists to close.
//!
//! The envelope wraps the record body in `{schema, content_hash, id, body}`:
//!
//! * **`content_hash`** — sha256 over the EXACT `body` bytes. Catches bit-rot
//!   and truncation: any mutation of the body fails the hash, so the corrupt
//!   record is a LOUD reject (the caller's tolerant-skip / rollback arm),
//!   never a trusted parse.
//! * **`id`** — the record's identity (its filename stem for `durable_record`,
//!   the `sandbox_id` for a spool). Verified against the id EXPECTED at the
//!   path, so a MISDIRECTED read — a lying disk serving another slot's
//!   validly-sealed bytes for this path — is caught (the hash alone cannot:
//!   the other record is internally consistent).
//!
//! This is a **clean-break format bump** (repo philosophy: no compat shim).
//! These files are HOST-LOCAL transient state — written and read only by the
//! same host's successor process through this crate's `load_all` / `read_spool`
//! (no cross-host or cross-version reader; the coordinator holds PG rows, not
//! these files), so an old un-enveloped file is simply rejected as malformed
//! and re-derived. `schema` guards the format.
//!
//! **Residual (reported, NOT closed here):** a STALE read serving an EARLIER,
//! validly-sealed version of the SAME id passes both checks — the envelope is
//! internally consistent. Detecting that needs a monotonic write-generation
//! bound cross-checked against durable state (a larger change; the spool's
//! lineage version-gate partially covers it). The chunk-store manifest
//! content-digest is a cross-system (GCS + coordinator) format and is likewise
//! out of scope — see the PR body's design sketch.

use engram_chunk_store::ChunkHash;
use serde::{Deserialize, Serialize};

/// The envelope format version. A clean-break bump; there is no v0 reader.
pub const ENVELOPE_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Envelope {
    /// Format guard — an old un-enveloped file (or a future version) fails to
    /// deserialize as this and is rejected as malformed.
    schema: u32,
    /// sha256 over `body`'s exact bytes.
    content_hash: ChunkHash,
    /// The record's identity (filename stem / sandbox id) — binds the sealed
    /// bytes to their slot so a misdirected read is caught, not just a flip.
    id: String,
    /// The exact serialized record JSON.
    body: String,
}

/// Why [`open`] rejected an envelope. Each variant maps to a DISTINCT caller
/// log so a checksum failure (bit-rot) is never confused with a torn write
/// (a legitimate crash mid-`persist`) or a misdirected read.
#[derive(Debug)]
pub enum OpenError {
    /// Not a well-formed envelope — torn/truncated, or a pre-envelope file.
    Malformed(serde_json::Error),
    /// The body's hash does not match the sealed hash — bit-rot / a lying
    /// fsync mutated the stored bytes.
    ChecksumMismatch { expected: String, got: String },
    /// The sealed id does not match the id expected at this path — a
    /// misdirected read serving another slot's (validly-sealed) bytes.
    IdMismatch { expected: String, sealed: String },
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Malformed(e) => write!(f, "malformed envelope: {e}"),
            OpenError::ChecksumMismatch { expected, got } => {
                write!(
                    f,
                    "content checksum mismatch (sealed {expected}, bytes {got})"
                )
            }
            OpenError::IdMismatch { expected, sealed } => {
                write!(
                    f,
                    "identity mismatch (expected {expected}, sealed {sealed})"
                )
            }
        }
    }
}

impl std::error::Error for OpenError {}

/// Seal `body` (the exact record JSON) under `id`. Returns the envelope bytes
/// to write to disk. Serialization is infallible (a `String` + a hash + a
/// `u32`), so this does not return a `Result`.
pub fn seal(id: &str, body: &str) -> Vec<u8> {
    let env = Envelope {
        schema: ENVELOPE_SCHEMA,
        content_hash: ChunkHash::of(body.as_bytes()),
        id: id.to_string(),
        body: body.to_string(),
    };
    serde_json::to_vec_pretty(&env).expect("envelope serialization is infallible")
}

/// Open an envelope, verifying the schema, the content hash, AND the identity.
/// Returns the body bytes (the caller then parses them into the record type).
pub fn open(bytes: &[u8], expected_id: &str) -> Result<Vec<u8>, OpenError> {
    let env: Envelope = serde_json::from_slice(bytes).map_err(OpenError::Malformed)?;
    if env.schema != ENVELOPE_SCHEMA {
        // A schema we cannot vouch for is treated as malformed — no partial
        // trust of an unknown format.
        return Err(OpenError::Malformed(serde::de::Error::custom(format!(
            "unknown envelope schema {}",
            env.schema
        ))));
    }
    let got = ChunkHash::of(env.body.as_bytes());
    if got != env.content_hash {
        return Err(OpenError::ChecksumMismatch {
            expected: env.content_hash.to_hex(),
            got: got.to_hex(),
        });
    }
    if env.id != expected_id {
        return Err(OpenError::IdMismatch {
            expected: expected_id.to_string(),
            sealed: env.id,
        });
    }
    Ok(env.body.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrips() {
        let body = r#"{"version":41,"note":"hello"}"#;
        let sealed = seal("rec-1", body);
        let got = open(&sealed, "rec-1").unwrap();
        assert_eq!(got, body.as_bytes());
    }

    #[test]
    fn a_flipped_body_byte_fails_the_checksum() {
        // The exact storage lie the envelope closes: mutate the body so it
        // STAYS valid JSON (a pre-envelope loader would trust it) but the
        // sealed hash no longer matches.
        let body = r#"{"version":41}"#;
        let sealed = seal("rec-1", body);
        let mut v: serde_json::Value = serde_json::from_slice(&sealed).unwrap();
        // A valid-JSON body with a different version — pre-envelope, trusted.
        v["body"] = serde_json::Value::String(r#"{"version":999}"#.into());
        let mangled = serde_json::to_vec(&v).unwrap();
        match open(&mangled, "rec-1") {
            Err(OpenError::ChecksumMismatch { .. }) => {}
            other => panic!("expected ChecksumMismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_misdirected_read_of_another_slots_bytes_fails_the_identity() {
        // Slot A's validly-sealed bytes served for slot B's path: the hash is
        // fine (internally consistent) but the identity binding catches it.
        let sealed_a = seal("slot-a", r#"{"n":1}"#);
        match open(&sealed_a, "slot-b") {
            Err(OpenError::IdMismatch { expected, sealed }) => {
                assert_eq!(expected, "slot-b");
                assert_eq!(sealed, "slot-a");
            }
            other => panic!("expected IdMismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_stale_but_validly_sealed_same_id_read_is_the_documented_residual() {
        // An EARLIER version of the SAME id is internally consistent and keeps
        // its id — the content-hash envelope cannot distinguish it. This test
        // PINS the residual so a future generation-bound change is a conscious
        // extension, not a surprise.
        let stale = seal("slot-a", r#"{"version":1}"#);
        assert!(
            open(&stale, "slot-a").is_ok(),
            "a stale same-id sealed record passes — the residual the module doc names",
        );
    }

    #[test]
    fn a_pre_envelope_raw_record_is_rejected_as_malformed() {
        let raw = r#"{"version":41}"#.as_bytes();
        match open(raw, "rec-1") {
            Err(OpenError::Malformed(_)) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }
}
