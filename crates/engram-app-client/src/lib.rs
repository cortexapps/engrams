//! Client for the orchestrator's app surface.
//!
//! The orchestrator is the product API: the dashboard, the `engrams` CLI and
//! `engrams tui` all talk to it and never to the coordinator. Its surface is
//!
//! - Connect RPC on `/rpc` over **HTTP/1.1** ([`connect`]) — chosen on purpose
//!   so every hop a client meets (dev proxies, port-forwards, the cloud load
//!   balancer) passes it; end-to-end gRPC h2 trailers would not;
//! - the SSE event tail `GET /api/v1/sessions/:id/events` ([`sse`]);
//! - the better-auth device-flow login routes ([`device_flow`]).
//!
//! Credentials live in `hosts.json` ([`config`]). The generated message types
//! are in [`proto`].

pub mod config;
pub mod connect;
pub mod device_flow;
pub mod error;
pub mod proto;
pub mod services;
pub mod shell;
pub mod sse;

pub use connect::{Auth, Transport};
pub use error::{Code, Error};
pub use services::Clients;
