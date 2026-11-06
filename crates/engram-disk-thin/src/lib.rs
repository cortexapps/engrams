//! [`ThinPoolDisk`]: the production [`SessionDisk`] on a dm-thin pool.
//!
//! The pool's metadata is the only record of which thins exist, and the
//! device-mapper table is the only record of which name maps to which
//! thin. The disk keeps no ledger of its own; after a restart `open`
//! reads the kernel. Device nodes live in a directory the disk owns, so
//! nothing depends on udev.

#[cfg(target_os = "linux")]
mod disk;
#[cfg(target_os = "linux")]
mod dm;
pub mod parse;

#[cfg(target_os = "linux")]
pub use disk::ThinPoolDisk;
