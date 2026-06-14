//! Compile the proto schemas into Rust tonic + prost bindings:
//!
//! - `host_service.proto` (ADR 0013) → `$OUT_DIR/engram.host.v1.rs`,
//!   `include!`'d from `src/grpc.rs`.
//! - `engram/app/v1/*.proto` (ADR 0051 control plane) →
//!   `$OUT_DIR/engram.app.v1.rs`, `include!`'d from `src/app.rs`.
//!   `task.proto` is deliberately absent: tasks are orchestrator-native
//!   (ADR 0051 §3) and the coordinator must not accrete task concepts.
//!
//! Reruns the codegen only when the proto files themselves change — the
//! `tonic-build` defaults already do this via cargo's
//! `rerun-if-changed` directive on the protos we pass.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "proto/host_service.proto",
        "proto/engram/app/v1/session.proto",
        "proto/engram/app/v1/fleet.proto",
        "proto/engram/app/v1/image.proto",
        "proto/engram/app/v1/secret.proto",
        // task.proto is deliberately absent: orchestrator-native (ADR §3).
    ];
    let includes = ["proto"];
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        // The default 4 MiB inbound message cap is fine for unary
        // payloads (SandboxSpec, SnapshotMetadata are well under
        // that). `ReapMaterializeDir` ships a vec of UUIDs that can
        // reach 16 MiB at 1M IDs; the host server bumps its decode
        // cap inline at the call site rather than globally here.
        .compile_protos(&protos, &includes)?;
    Ok(())
}
