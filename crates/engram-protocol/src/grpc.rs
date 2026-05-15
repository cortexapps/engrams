//! gRPC bindings generated from `proto/host_service.proto` (ADR 0013).
//!
//! Everything inside the included file is `pub`. Re-exported under the
//! `grpc` module so the rest of the crate (and downstream consumers)
//! says `engram_protocol::grpc::HostServiceClient` / `HostServiceServer`
//! / `CreateSandboxRequest` / etc.
//!
//! Codegen is driven by `build.rs` via `tonic-build`. The output path
//! `$OUT_DIR/engram.host.v1.rs` is derived from the proto's package
//! declaration `engram.host.v1` — keep them in sync.

#![allow(clippy::derive_partial_eq_without_eq)]
#![allow(clippy::large_enum_variant)]
tonic::include_proto!("engram.host.v1");
