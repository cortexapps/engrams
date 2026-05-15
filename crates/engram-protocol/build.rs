//! Compile the `host_service.proto` schema (ADR 0013) into Rust
//! tonic + prost bindings. Output goes to `$OUT_DIR/engram.host.v1.rs`
//! and is `include!`'d from `src/grpc.rs`.
//!
//! Reruns the codegen only when the proto file itself changes — the
//! `tonic-build` defaults already do this via cargo's
//! `rerun-if-changed` directive on the protos we pass.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = ["proto/host_service.proto"];
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
