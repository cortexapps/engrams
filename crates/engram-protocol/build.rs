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
//! `tonic-prost-build` defaults already do this via cargo's
//! `rerun-if-changed` directive on the protos we pass.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "proto/host_service.proto",
        "proto/engram/app/v1/session.proto",
        "proto/engram/app/v1/fleet.proto",
        "proto/engram/app/v1/image.proto",
        "proto/engram/app/v1/mount_catalog.proto",
        "proto/engram/app/v1/harness.proto",
        "proto/engram/app/v1/org_secret.proto",
        "proto/engram/app/v1/oauth.proto",
        "proto/engram/app/v1/mint.proto",
        "proto/engram/app/v1/integration_op.proto",
        // secret.proto removed in ADR 0051 Drip A: the orchestrator owns the
        // user's harness token; it rides CreateSession.harness_env now.
        // task.proto is deliberately absent: orchestrator-native (ADR §3).
        // integration.proto is deliberately absent: orchestrator-native connector
        // catalog CRUD (ADR 0057 C3) — the coordinator never sees connectors.
    ];
    let includes = ["proto"];
    // Emit a serialized FileDescriptorSet covering the compiled protos so
    // the coordinator can stand up tonic-reflection on its app-gRPC server
    // (grpcurl `list`/`describe`/proto-less calls against the
    // network-private control plane — schema only, no data). The bytes are
    // loaded back via `include_bytes!` in `src/app.rs`.
    let descriptor_path =
        std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("engram_app_descriptor.bin");
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .file_descriptor_set_path(&descriptor_path)
        // The default 4 MiB inbound message cap is fine for unary
        // payloads (SandboxSpec, SnapshotMetadata are well under
        // that). `ReapMaterializeDir` ships a vec of UUIDs that can
        // reach 16 MiB at 1M IDs; the host server bumps its decode
        // cap inline at the call site rather than globally here.
        .compile_protos(&protos, &includes)?;
    Ok(())
}
