//! gRPC bindings generated from `proto/engram/app/v1/*.proto`
//! (ADR 0051 control plane): session, fleet, image.
//!
//! `task.proto` is deliberately excluded from codegen — tasks are
//! orchestrator-native (ADR 0051 §3) and the coordinator must not
//! accrete task concepts. No `TaskService` types exist here.
//!
//! Everything inside the included file is `pub`. Re-exported under the
//! `app` module so downstream consumers say
//! `engram_protocol::app::SessionServiceServer` / `FleetServiceClient`
//! / `CreateSessionRequest` / etc.
//!
//! Codegen is driven by `build.rs` via `tonic-build`. The output path
//! `$OUT_DIR/engram.app.v1.rs` is derived from the protos' package
//! declaration `engram.app.v1` — keep them in sync.

#![allow(clippy::derive_partial_eq_without_eq)]
#![allow(clippy::large_enum_variant)]
tonic::include_proto!("engram.app.v1");
