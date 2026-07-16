//! Dev-only test support (ADR 0099).
//!
//! This crate is only ever a dev-dependency; nothing here may be reached
//! from production code. It exists so isolation/fault-injection utilities
//! have one home instead of being copy-pasted between test binaries.

pub mod pg;
