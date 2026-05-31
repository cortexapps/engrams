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

pub mod cacerts;
pub mod clock;
pub mod forge;
pub mod handler;
pub mod harness_supervisor;
pub mod proto;
pub mod shell;

pub use cacerts::{CaCertInstaller, CaCertPaths};
pub use handler::serve_connection;
pub use harness_supervisor::HarnessSupervisor;
pub use proto::{
    read_msg, write_msg, AgentReady, InstallHostCaRequest, SpawnHarnessRequest,
    WireDownloadResponse, WireExecEvent, WireExecRequest, WireHandshake, WireHandshakeAck,
    WireRequest, WireResponse, WireStatResponse, ENGRAM_AGENTD_READY_PORT, MAX_MSG_BYTES,
};
