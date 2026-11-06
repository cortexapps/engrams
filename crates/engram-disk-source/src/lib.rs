//! A read-only block device whose bytes are one disk manifest (ADR 0127).
//!
//! [`DiskReader`] maps a read at any offset to the manifest's chunks and
//! runs on every platform, as does [`policy`], which decides when a
//! failed read is attempted again. [`ublk`] serves it as `/dev/ublkbN`
//! on Linux.

pub mod policy;
pub mod reader;
#[cfg(target_os = "linux")]
pub mod ublk;

pub use reader::{DiskReader, LocalBase};
