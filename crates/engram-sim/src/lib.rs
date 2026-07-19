//! Deterministic simulation substrate (ADR 0098 D4).
//!
//! Three building blocks the DST harness (`engram-dst`, D5) composes,
//! usable standalone by coordinator unit tests today:
//!
//! - [`SimClock`] — a [`engram_core::traits::Clock`] that is a thin view
//!   over tokio's *paused* clock: virtual time moves only when the test
//!   or scheduler advances it.
//! - [`SimEntropy`] — a seeded [`engram_core::traits::Entropy`]: the same
//!   seed replays the same id sequence.
//! - [`meta::SimMetadataStore`] — an in-memory `MetadataStore` faithful to
//!   `PostgresStore`'s observable semantics, kept honest by the
//!   conformance suite (`tests/`), which runs every scenario against BOTH
//!   stores.
//!
//! **Process rule (ADR 0098 D4):** any PR that adds a `MetadataStore`
//! method or changes `PostgresStore` SQL semantics must extend the
//! conformance suite in the same PR.

pub mod clock;
pub mod entropy;
pub mod meta;

pub use clock::{ManualClock, SimClock};
pub use entropy::SimEntropy;
pub use meta::{pending_counts, SimMetadataStore};
