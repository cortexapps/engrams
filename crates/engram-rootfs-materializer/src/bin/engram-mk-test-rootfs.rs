//! ADR 0096: build a bootable TEST rootfs ext4 from a plain directory
//! tree — Docker-free, mount-free, root-free, runs on macOS. This is
//! what lets the live VZ e2e (`e2e_vz.rs`) stage a *current* guest
//! image on a dev Mac or a Docker-less CI runner instead of skipping.
//!
//! The tree is expected to be an extracted Alpine minirootfs (a
//! complete ~3 MB busybox userland — `make-test-rootfs.sh` downloads
//! and verifies the pinned tarball). This bin then:
//!   1. installs any `--install name=path` extras into `usr/bin/`
//!      (e.g. the cross-built `vz-e2e-echo` guest test helper),
//!   2. normalizes the boot contract dirs the stage-1 shim relies on
//!      (`/proc /sys /dev /run /tmp(1777) /etc /root /workspace`),
//!   3. injects the REAL prod stage-1 init shim (`inject_init`,
//!      `Transport::Vsock`) — the e2e boots the same init contract as
//!      a materialized session image, including the "no agentd baked
//!      in; exec it from the bundle slot" rule (ADR 0080),
//!   4. packs it with `stream_pack::pack_tree` (mkext4, ADR 0093).
//!
//! Usage:
//!   engram-mk-test-rootfs --tree <extracted-rootfs-dir> --out <img.ext4> \
//!       [--install vz-e2e-echo=/path/to/vz-e2e-echo]...

use std::path::PathBuf;

use engram_rootfs_materializer::inject::{inject_init, InitInjection, Transport};
use engram_rootfs_materializer::stream_pack::pack_tree;

/// Agentd's guest vsock exec port. Mirrors the backends' own local
/// `ENGRAM_AGENTD_PORT = 1024` consts (the value is part of the
/// host↔guest wire contract; see `InitInjection::vsock_port`).
const AGENTD_VSOCK_PORT: u32 = 1024;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut tree: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut installs: Vec<(String, PathBuf)> = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = |flag: &str| {
            args.next()
                .unwrap_or_else(|| die(&format!("{flag} needs a value")))
        };
        match a.as_str() {
            "--tree" => tree = Some(PathBuf::from(val("--tree"))),
            "--out" => out = Some(PathBuf::from(val("--out"))),
            "--install" => {
                let v = val("--install");
                let (name, path) = v
                    .split_once('=')
                    .unwrap_or_else(|| die("--install expects name=/host/path"));
                installs.push((name.to_string(), PathBuf::from(path)));
            }
            other => die(&format!("unknown arg: {other}")),
        }
    }
    let tree = tree.unwrap_or_else(|| die("--tree is required"));
    let out = out.unwrap_or_else(|| die("--out is required"));
    if !tree.is_dir() {
        die(&format!("--tree {} is not a directory", tree.display()));
    }

    // 1. Extras into usr/bin (0755).
    for (name, src) in &installs {
        let dst_dir = tree.join("usr/bin");
        std::fs::create_dir_all(&dst_dir).expect("create usr/bin");
        let dst = dst_dir.join(name);
        std::fs::copy(src, &dst).unwrap_or_else(|e| {
            die(&format!(
                "install {} -> {}: {e}",
                src.display(),
                dst.display()
            ))
        });
        set_mode(&dst, 0o755);
    }

    // 2. Boot-contract dirs. The shim `mount`s over /proc /sys /dev
    //    /run and mkdir-p's the rest, but the MOUNT POINTS themselves
    //    must exist in the image. /tmp must be the world-writable
    //    sticky 1777 (the shim re-chmods, but bake it right too).
    for d in [
        "proc",
        "sys",
        "dev",
        "run",
        "tmp",
        "etc",
        "root",
        "workspace",
        "opt",
    ] {
        std::fs::create_dir_all(tree.join(d)).expect("create contract dir");
    }
    set_mode(&tree.join("tmp"), 0o1777);

    // 3. The real prod init shim, vsock transport (both backends).
    inject_init(
        &tree,
        &InitInjection {
            vsock_port: AGENTD_VSOCK_PORT,
            transport: Transport::Vsock,
            init_script: None,
        },
    )
    .await
    .expect("inject_init");

    // 4. Deterministic ext4 (mkext4 sizes it with `recommended_size`
    //    headroom, so the guest has room for its writes).
    let len = pack_tree(&tree, &out).unwrap_or_else(|e| die(&format!("pack_tree: {e}")));
    eprintln!(
        "engram-mk-test-rootfs: packed {} ({} MiB)",
        out.display(),
        len / (1024 * 1024)
    );
}

fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

fn die(msg: &str) -> ! {
    eprintln!("engram-mk-test-rootfs: {msg}");
    std::process::exit(2);
}
