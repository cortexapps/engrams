//! Wire types for the coordinator ↔ host channel (ADR 0013).
//!
//! Transport is HTTP/2 + gRPC (coord → host) via [`grpc_client`] +
//! [`grpc_pool`] on the coord side, served by the host-agent's
//! gRPC server. The proto schema lives at `proto/host_service.proto`
//! and generates into [`grpc`].
//!
//! Host → coord traffic (heartbeat, registry-auth, harness events,
//! idle-eviction-candidates, register) is plain HTTP/JSON — the
//! payload types ([`heartbeat::HostCapacityReport`] etc.) are
//! defined here for sharing across both sides.
//!
//! Pre-ADR-0013 this crate held a full bincode-over-WebSocket
//! frame protocol; that's been retired and only the shared
//! payload shapes remain.

pub mod admin;
pub mod app;
pub mod grpc;
pub mod grpc_client;
pub mod grpc_pool;
pub mod heartbeat;
pub mod wire;

pub use admin::HostAdminHandler;
pub use heartbeat::*;
pub use wire::{
    WireExecRequest, WireReapStats, WireWriteFilesRequest, WireWriteFilesResponse, WIRE_VERSION,
};
