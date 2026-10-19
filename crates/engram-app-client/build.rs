//! Compile the orchestrator app contract (`engram/app/v1/*.proto`) into
//! prost message types: `$OUT_DIR/engram.app.v1.rs`, `include!`'d from
//! `src/proto.rs`.
//!
//! Messages only — no tonic services. The client speaks the Connect
//! protocol over HTTP/1.1 (see `src/connect.rs`), so the service and method
//! names are plain strings at the call site. This set is the CLIENT's view of
//! the contract: it includes `task.proto`, `profile.proto` and
//! `api_key.proto`, which `engram-protocol` deliberately leaves out because
//! the coordinator must not accrete orchestrator-native concepts.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = "../engram-protocol/proto";
    let protos = [
        "engram/app/v1/session.proto",
        "engram/app/v1/oauth.proto",
        "engram/app/v1/fleet.proto",
        "engram/app/v1/image.proto",
        "engram/app/v1/profile.proto",
        "engram/app/v1/user.proto",
        "engram/app/v1/task.proto",
        "engram/app/v1/api_key.proto",
    ]
    .map(|p| format!("{root}/{p}"));
    // prost-build registers rerun-if-changed for the files it is handed;
    // the directory covers imports those files pull in.
    println!("cargo:rerun-if-changed={root}/engram/app/v1");
    // BTreeMap for every `map<>` field: the CLI serializes maps (image env,
    // profile env_vars) and a HashMap would make `--json` output
    // non-deterministic between runs.
    prost_build::Config::new()
        .btree_map(["."])
        .compile_protos(&protos, &[root])?;
    Ok(())
}
