//! macOS Apple Silicon `SandboxBackend` backed by Apple's
//! Virtualization.framework (henceforth "VZ").
//!
//! This is the sister to `engram-sandbox-firecracker` for hosts that
//! can't run KVM. It mirrors FC's external surface — real virtio-vsock
//! (`VZVirtioSocketDevice`, ADR 0066 Phase 2) with agentd exposed as a
//! UDS file at `<work_dir>/<sid>.vsock_1024`, the harness/upload/relay
//! channels served over the same vsock device, SpawnHarness handshake
//! against agentd, harness sink fanout, snapshot/restore — so the rest
//! of the stack runs unchanged when the coordinator picks
//! `--sandbox-backend=vz`.
//!
//! # Why in-process
//!
//! Earlier sketches proposed a separate Swift driver binary the Rust
//! crate would shell out to. We use the `objc2-virtualization`
//! crate to call VZ APIs directly from Rust instead — same address
//! space as the rest of the coordinator. Tradeoff: a VZ panic takes
//! the coord down (acceptable; same blast radius as any other
//! coord-internal panic). Win: one language, one toolchain, one
//! debugger, no IPC layer to maintain.
//!
//! # Cross-platform surface
//!
//! On non-macOS hosts this crate compiles to an empty shell — useful
//! so workspace `cargo check` stays green on Linux/CI without any
//! `cfg`-juggling at the call site. The coordinator's runtime
//! `match cli.sandbox_backend` gates `VzBackend` construction
//! behind the same `target_os = "macos"` check; non-macOS hosts
//! that try to select `--sandbox-backend=vz` get a clean config
//! error rather than a link error.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(target_os = "macos")]
mod backend;

#[cfg(target_os = "macos")]
mod vm;

#[cfg(target_os = "macos")]
mod vsock_bridge;

#[cfg(target_os = "macos")]
mod disk;

#[cfg(target_os = "macos")]
mod snapshot;

#[cfg(target_os = "macos")]
pub use backend::{VzBackend, VzConfig};

#[cfg(not(target_os = "macos"))]
mod stub {
    use std::path::PathBuf;

    /// Stub on non-macOS so library consumers can name the type
    /// behind a `cfg` without conditional `use`. Constructing it
    /// is impossible — every constructor returns an error — so
    /// the only way to actually run a VZ backend is on macOS.
    pub struct VzBackend {
        _private: (),
    }

    /// Stub on non-macOS. Mirrors the macOS shape.
    #[derive(Clone, Debug)]
    pub struct VzConfig {
        pub kernel_path: PathBuf,
        pub memory_mb: u64,
        pub vcpus: u32,
    }

    impl VzBackend {
        pub fn new(_work_dir: PathBuf, _cfg: VzConfig) -> Result<Self, engram_core::SandboxError> {
            Err(engram_core::SandboxError::InvalidSpec(
                "engram-sandbox-vz only runs on macOS — pick --sandbox-backend=process or \
                 --sandbox-backend=firecracker on this host"
                    .into(),
            ))
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub use stub::{VzBackend, VzConfig};
