//! Wire types for the coordinator <-> host channel.
//!
//! Phase 3 transport: bincode-encoded [`Frame`]s carried inside binary
//! WebSocket messages. The [`wire`] module defines the frame schema;
//! [`codec`] handles encode/decode against `tokio_tungstenite::Message`;
//! [`client`] holds the request-id demuxer and the [`RemoteSandboxBackend`]
//! impl; [`server`] is the host-side accept loop.
//!
//! Phase 1+2 also defined `Heartbeat` / `AssignSession` / `RevokeSession`
//! shapes used in-process before any wire was needed; those still live
//! in [`heartbeat`] / [`scheduling`] and are now embedded in the [`wire`]
//! frame schema.

pub mod client;
pub mod codec;
pub mod grpc;
pub mod grpc_client;
pub mod grpc_pool;
pub mod heartbeat;
pub mod scheduling;
pub mod server;
pub mod wire;

pub use client::HostRequestHandler;
pub use heartbeat::*;
pub use scheduling::*;
pub use wire::{
    Frame, NotifyKind, RegistryCreds, RemoteError, RequestKind, ResponseKind, StreamItem,
    TraceContext, WireExecRequest, WIRE_VERSION,
};
