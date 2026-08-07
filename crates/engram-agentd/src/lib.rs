//! In-guest exec daemon for Firecracker microVMs. Listens for host
//! connections (over vsock in production, plain UDS for tests), runs
//! one command per connection, and streams stdout/stderr/exit back
//! over the same connection.
//!
//! The crate exposes two surfaces:
//!
//! - [`proto`] — wire types ([`WireExecRequest`], [`WireExecEvent`]) and
//!   the length-prefixed bincode framing helpers. Both the agent and
//!   the host (`engram-sandbox-firecracker`) depend on this.
//! - [`handler::serve_connection`] — runs one exec to completion against
//!   any `AsyncRead + AsyncWrite` stream. The binary's accept loop
//!   wraps this; tests can drive it over a `tokio::io::duplex` pipe.
//!
//! `main.rs` ties them together with a CLI for the in-guest binary.

pub mod browser;
pub mod cacerts;
pub mod clock;
pub mod exec_journal;
pub mod forge;
pub mod handler;
pub mod harness_supervisor;
pub mod ide;
pub mod port_relay;
pub mod proto;
pub mod reaper;
pub mod refresh;
pub mod remount;
pub mod share;
pub mod shell;
pub mod swap;
pub mod tuning;
// `pub` so the `engram-agentd` binary (main.rs) shares this one module rather
// than recompiling its own copy — the bin's readiness dial reads
// `time_source::metrics_now`, and a second `mod time_source` in main.rs would
// flag the lib-only `metrics_now_tokio` as dead code in the bin build.
pub mod time_source;

pub use cacerts::{CaCertInstaller, CaCertPaths};
pub use handler::{serve_connection, serve_connection_with_journal};
pub use harness_supervisor::HarnessSupervisor;
pub use proto::{
    read_msg, write_msg, AgentReady, SpawnHarnessRequest, WireDownloadResponse, WireExecEvent,
    WireExecRequest, WireHandshake, WireHandshakeAck, WireRequest, WireResponse, WireStatResponse,
    ENGRAM_AGENTD_READY_PORT, MAX_MSG_BYTES,
};
