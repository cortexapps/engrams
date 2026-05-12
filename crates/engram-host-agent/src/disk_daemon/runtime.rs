//! Linux-only NBD server loop + kernel `/dev/nbdN` orchestration.
//!
//! Stub — actual implementation lands in the next slice (NBD server
//! loop + kernel ioctl wrappers). Keeping the file in place so the
//! `#[cfg(target_os = "linux")]` mod gate in `mod.rs` resolves.

#![cfg(target_os = "linux")]
