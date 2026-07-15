//! ADR 0080 phase-3b pre-merge validation: golden-diff the flattener
//! against `docker export` for a REAL image.
//!
//! `#[ignore]`'d and env-driven — this is a manual validation harness
//! (run on the real-KVM dev VM against a local registry), not a CI
//! test. Usage:
//!
//! ```text
//! # on the VM: push the image to a local registry, then
//! docker create --name gd <image> true
//! docker export gd -o /tmp/gd-<image>.tar
//! ENGRAM_GOLDEN_DIFF_URI=127.0.0.1:5010/<image>:tag \
//! ENGRAM_GOLDEN_DIFF_EXPORT_TAR=/tmp/gd-<image>.tar \
//! cargo test -p engram-rootfs-materializer --test golden_diff -- --ignored --nocapture
//! ```
//!
//! What it does:
//! 1. `pull_image` + `flatten::apply_layer` (the exact per-layer
//!    decompressor match from `Materializer::materialize`) into a kept
//!    scratch tree — the pipeline STOPPED after flatten, which is the
//!    stage that golden-diffs against `docker export`.
//! 2. Attempts `TreeMetadata::apply_ownership` (real `lchown`): as
//!    root it must succeed; unprivileged it must degrade to
//!    PermissionDenied (both outcomes reported).
//! 3. Reads the `docker export` tar IN-PROCESS (no extraction — so
//!    device nodes and foreign uid/gids are compared faithfully even
//!    when running unprivileged) and diffs:
//!    file list + entry types, modes (incl. setuid/setgid/sticky),
//!    symlink targets, hardlink groupings, per-file sha256, uid/gid
//!    (export-tar header vs the TreeMetadata sidecar, always; vs the
//!    on-disk tree too when ownership was applied), and xattrs.
//!
//! Known-acceptable deltas (reported, not failed):
//! - `/.dockerenv` — docker adds it at create time.
//! - `/dev/**` — docker adds console/pts/shm stubs; the flatten skips
//!   device nodes/fifos by design (devtmpfs provides /dev at boot).
//!   Both sides' inventories are printed for assessment.
//! - `/proc`, `/sys` — empty mount-point dirs.
//! - `/etc/{hostname,hosts,resolv.conf,mtab}` — docker mutates or
//!   creates these at container-create time.
//! - mtimes — not compared (the pack stage clamps them separately).
//! - xattrs the unprivileged flatten recorded in `skipped_xattrs`.
//!
//! Anything else is a REAL diff: printed verbatim, and the test fails.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use engram_oci::{AnonymousResolver, OciClient};
use engram_rootfs_materializer::{
    apply_layer, pull_image, LayerCompression, Platform, TreeMetadata,
};
use sha2::Digest as _;

// ---------------------------------------------------------------
// Entry model (shared by both sides of the diff).
// ---------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
enum Kind {
    Dir,
    File,
    Symlink,
    Char(u64, u64),
    Block(u64, u64),
    Fifo,
}

#[derive(Clone, Debug)]
struct Entry {
    kind: Kind,
    mode: u32,
    uid: u64,
    gid: u64,
    size: u64,
    sha256: Option<String>,
    /// Symlink target, verbatim.
    link: Option<String>,
    /// Hardlink group representative (lexicographically smallest path
    /// sharing the inode / tar link target), if in a group of >= 2.
    hardlink_rep: Option<String>,
    xattrs: BTreeMap<String, Vec<u8>>,
}

fn norm_tar_path(raw: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for comp in raw.components() {
        match comp {
            Component::Normal(c) => parts.push(c.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return None, // absolute / .. — docker export never emits these
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

// ---------------------------------------------------------------
// Side A: the docker-export tar, read in-process.
// ---------------------------------------------------------------

fn read_export_tar(path: &Path) -> BTreeMap<String, Entry> {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {path:?}: {e}"));
    let mut archive = tar::Archive::new(std::io::BufReader::new(file));
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    // path -> tar hardlink target (normalized)
    let mut links: Vec<(String, String)> = Vec::new();

    for entry in archive.entries().expect("tar entries") {
        let mut entry = entry.expect("tar entry");
        let raw = entry.path().expect("entry path").into_owned();
        let Some(rel) = norm_tar_path(&raw) else {
            continue;
        };
        let h = entry.header();
        let mode = h.mode().unwrap_or(0) & 0o7777;
        let uid = h.uid().unwrap_or(0);
        let gid = h.gid().unwrap_or(0);
        let size = h.size().unwrap_or(0);
        let entry_type = h.entry_type();
        let dev_major = h.device_major().ok().flatten().unwrap_or(0) as u64;
        let dev_minor = h.device_minor().ok().flatten().unwrap_or(0) as u64;

        let mut xattrs = BTreeMap::new();
        if let Ok(Some(exts)) = entry.pax_extensions() {
            for ext in exts.flatten() {
                if let Ok(key) = ext.key() {
                    if let Some(name) = key.strip_prefix("SCHILY.xattr.") {
                        xattrs.insert(name.to_string(), ext.value_bytes().to_vec());
                    }
                }
            }
        }

        use tar::EntryType;
        let (kind, sha256, link) = match entry_type {
            EntryType::Directory => (Kind::Dir, None, None),
            EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => {
                let mut hasher = sha2::Sha256::new();
                std::io::copy(&mut entry, &mut hasher).expect("hash tar entry");
                (Kind::File, Some(format!("{:x}", hasher.finalize())), None)
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .expect("link name")
                    .expect("symlink target")
                    .to_string_lossy()
                    .into_owned();
                (Kind::Symlink, None, Some(target))
            }
            EntryType::Link => {
                let target = entry
                    .link_name()
                    .expect("link name")
                    .expect("hardlink target")
                    .into_owned();
                let target = norm_tar_path(&target).expect("hardlink target path");
                links.push((rel.clone(), target));
                // filled in after the pass (content/kind come from the target)
                entries.insert(
                    rel,
                    Entry {
                        kind: Kind::File,
                        mode,
                        uid,
                        gid,
                        size: 0,
                        sha256: None,
                        link: None,
                        hardlink_rep: None,
                        xattrs,
                    },
                );
                continue;
            }
            EntryType::Char => (Kind::Char(dev_major, dev_minor), None, None),
            EntryType::Block => (Kind::Block(dev_major, dev_minor), None, None),
            EntryType::Fifo => (Kind::Fifo, None, None),
            _ => continue,
        };
        entries.insert(
            rel,
            Entry {
                kind,
                mode,
                uid,
                gid,
                size,
                sha256,
                link,
                hardlink_rep: None,
                xattrs,
            },
        );
    }

    // Resolve hardlink groups: union {path, target} chains; the group
    // representative is the smallest path; every member inherits the
    // representative's content hash / size / kind.
    // (tar hardlink targets always name a previously-emitted real file.)
    let mut groups: HashMap<String, BTreeSet<String>> = HashMap::new();
    for (path, target) in &links {
        // Follow one level of link-to-link chains.
        let mut root = target.clone();
        let mut seen = 0;
        while let Some((_, t)) = links.iter().find(|(p, _)| p == &root) {
            root = t.clone();
            seen += 1;
            assert!(seen < 100, "hardlink chain loop in export tar at {path}");
        }
        let set = groups.entry(root.clone()).or_default();
        set.insert(root.clone());
        set.insert(path.clone());
    }
    for set in groups.values() {
        let rep = set.iter().next().unwrap().clone();
        let canonical = set
            .iter()
            .find_map(|p| {
                let e = entries.get(p)?;
                e.sha256.as_ref()?;
                Some((e.kind.clone(), e.sha256.clone(), e.size))
            })
            .unwrap_or((Kind::File, None, 0));
        for p in set {
            if let Some(e) = entries.get_mut(p) {
                e.hardlink_rep = Some(rep.clone());
                e.kind = canonical.0.clone();
                e.sha256 = canonical.1.clone();
                e.size = canonical.2;
            }
        }
    }
    entries
}

// ---------------------------------------------------------------
// Side B: the flattened tree on disk.
// ---------------------------------------------------------------

fn read_tree(root: &Path) -> BTreeMap<String, Entry> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let mut out = BTreeMap::new();
    let mut by_inode: HashMap<(u64, u64), BTreeSet<String>> = HashMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let p = entry.path();
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let meta = std::fs::symlink_metadata(&p).expect("lstat");
            let ft = meta.file_type();
            let mode = meta.permissions().mode() & 0o7777;
            let (kind, sha256, link) = if ft.is_dir() {
                stack.push(p.clone());
                (Kind::Dir, None, None)
            } else if ft.is_symlink() {
                let t = std::fs::read_link(&p).expect("readlink");
                (Kind::Symlink, None, Some(t.to_string_lossy().into_owned()))
            } else if ft.is_file() {
                let mut f = std::fs::File::open(&p).expect("open");
                let mut hasher = sha2::Sha256::new();
                std::io::copy(&mut f, &mut hasher).expect("hash");
                if meta.nlink() > 1 {
                    by_inode
                        .entry((meta.dev(), meta.ino()))
                        .or_default()
                        .insert(rel.clone());
                }
                (Kind::File, Some(format!("{:x}", hasher.finalize())), None)
            } else if ft.is_char_device() {
                (Kind::Char(0, 0), None, None)
            } else if ft.is_block_device() {
                (Kind::Block(0, 0), None, None)
            } else if ft.is_fifo() {
                (Kind::Fifo, None, None)
            } else {
                continue;
            };
            let mut xattrs = BTreeMap::new();
            if !ft.is_symlink() {
                if let Ok(names) = xattr::list(&p) {
                    for name in names {
                        let n = name.to_string_lossy().into_owned();
                        if let Ok(Some(v)) = xattr::get(&p, &name) {
                            xattrs.insert(n, v);
                        }
                    }
                }
            }
            out.insert(
                rel,
                Entry {
                    kind,
                    mode,
                    uid: meta.uid() as u64,
                    gid: meta.gid() as u64,
                    size: meta.len(),
                    sha256,
                    link,
                    hardlink_rep: None,
                    xattrs,
                },
            );
        }
    }
    for set in by_inode.values() {
        if set.len() < 2 {
            continue; // nlink>1 but the sibling is outside the tree? shouldn't happen
        }
        let rep = set.iter().next().unwrap().clone();
        for p in set {
            if let Some(e) = out.get_mut(p) {
                e.hardlink_rep = Some(rep.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------
// Classification of known-acceptable delta paths.
// ---------------------------------------------------------------

#[derive(Debug, Eq, PartialEq)]
enum Class {
    Compare,
    DockerEnv,
    Dev,
    ProcSys,
    MutableEtc,
}

fn classify(path: &str) -> Class {
    if path == ".dockerenv" {
        return Class::DockerEnv;
    }
    if path == "dev" || path.starts_with("dev/") {
        return Class::Dev;
    }
    if path == "proc" || path == "sys" || path.starts_with("proc/") || path.starts_with("sys/") {
        return Class::ProcSys;
    }
    if matches!(
        path,
        "etc/hostname" | "etc/hosts" | "etc/resolv.conf" | "etc/mtab"
    ) {
        return Class::MutableEtc;
    }
    Class::Compare
}

fn kind_str(k: &Kind) -> String {
    match k {
        Kind::Dir => "dir".into(),
        Kind::File => "file".into(),
        Kind::Symlink => "symlink".into(),
        Kind::Char(a, b) => format!("char({a},{b})"),
        Kind::Block(a, b) => format!("block({a},{b})"),
        Kind::Fifo => "fifo".into(),
    }
}

// ---------------------------------------------------------------
// The test.
// ---------------------------------------------------------------

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

fn platform() -> Platform {
    match env("ENGRAM_GOLDEN_DIFF_PLATFORM").as_deref() {
        Some("arm64") => Platform::LinuxArm64,
        Some("amd64") => Platform::LinuxAmd64,
        Some(other) => panic!("ENGRAM_GOLDEN_DIFF_PLATFORM must be amd64|arm64, got {other}"),
        None => {
            if cfg!(target_arch = "aarch64") {
                Platform::LinuxArm64
            } else {
                Platform::LinuxAmd64
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual golden-diff validation; needs ENGRAM_GOLDEN_DIFF_URI + ENGRAM_GOLDEN_DIFF_EXPORT_TAR"]
async fn golden_diff_flatten_vs_docker_export() {
    let uri = env("ENGRAM_GOLDEN_DIFF_URI")
        .expect("set ENGRAM_GOLDEN_DIFF_URI (e.g. 127.0.0.1:5010/debian:bookworm-slim)");
    let export_tar = PathBuf::from(
        env("ENGRAM_GOLDEN_DIFF_EXPORT_TAR").expect("set ENGRAM_GOLDEN_DIFF_EXPORT_TAR"),
    );
    let platform = platform();

    // --- 1. pull + flatten (the materialize pipeline's first half) ---
    let scratch = tempfile::tempdir().expect("scratch");
    let layers_dir = scratch.path().join("layers");
    let rootfs = scratch.path().join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();

    let client = OciClient::new(std::sync::Arc::new(AnonymousResolver));
    let pulled = pull_image(&client, &uri, platform, &layers_dir)
        .await
        .expect("pull_image");
    eprintln!(
        "pulled {} ({platform}): {} layers, {} compressed bytes, manifest {}",
        uri,
        pulled.layers.len(),
        pulled.compressed_bytes,
        pulled.manifest_digest
    );

    let mut tree_meta = TreeMetadata::default();
    for layer in &pulled.layers {
        let file = std::fs::File::open(&layer.path).expect("open layer");
        let reader = std::io::BufReader::new(file);
        match layer.compression {
            LayerCompression::Gzip => apply_layer(
                &rootfs,
                &mut tree_meta,
                flate2::read::GzDecoder::new(reader),
            ),
            LayerCompression::Zstd => apply_layer(
                &rootfs,
                &mut tree_meta,
                zstd::stream::read::Decoder::with_buffer(reader).expect("zstd decoder"),
            ),
            LayerCompression::None => apply_layer(&rootfs, &mut tree_meta, reader),
        }
        .unwrap_or_else(|e| panic!("apply_layer {}: {e}", layer.digest));
    }

    eprintln!("\n=== GOLDEN DIFF REPORT: {uri} ===");
    eprintln!(
        "flatten: {} sidecar entries, {} skipped specials, {} skipped xattrs",
        tree_meta.len(),
        tree_meta.skipped_specials.len(),
        tree_meta.skipped_xattrs.len()
    );
    eprintln!("-- flatten skipped_specials (device nodes / fifos; mknod needs root):");
    for p in &tree_meta.skipped_specials {
        eprintln!("   {p}");
    }
    eprintln!("-- flatten skipped_xattrs:");
    for x in &tree_meta.skipped_xattrs {
        eprintln!("   {} xattr {}: {}", x.path, x.name, x.error);
    }

    // --- 2. the real lchown path ---
    let ownership_applied = match tree_meta.apply_ownership(&rootfs) {
        Ok(()) => {
            eprintln!("-- ownership: APPLIED to the tree (running privileged)");
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!(
                "-- ownership: recorded-only (unprivileged; lchown refused: {e}); \
                 on-disk uid/gid NOT compared — sidecar-vs-export-tar comparison below \
                 covers ownership"
            );
            false
        }
        Err(e) => panic!("apply_ownership failed with a non-permission error: {e}"),
    };
    if env("ENGRAM_GOLDEN_DIFF_REQUIRE_OWNERSHIP").is_some() {
        assert!(
            ownership_applied,
            "ENGRAM_GOLDEN_DIFF_REQUIRE_OWNERSHIP set but apply_ownership degraded — \
             run this as root"
        );
    }

    // --- 3. read both sides ---
    let export = read_export_tar(&export_tar);
    let tree = read_tree(&rootfs);
    eprintln!(
        "export tar: {} entries; flatten tree: {} entries",
        export.len(),
        tree.len()
    );

    let mut real: Vec<String> = Vec::new();
    let mut acceptable: Vec<String> = Vec::new();
    let mut dev_inventory_export: Vec<String> = Vec::new();

    // --- 4a. export-side walk ---
    for (path, exp) in &export {
        let class = classify(path);
        let got = tree.get(path);
        match class {
            Class::DockerEnv => {
                acceptable.push(format!("[docker-added] {path}"));
                continue;
            }
            Class::Dev => {
                dev_inventory_export.push(format!(
                    "{} {} mode={:o} uid={} gid={} {}",
                    kind_str(&exp.kind),
                    path,
                    exp.mode,
                    exp.uid,
                    exp.gid,
                    if got.is_some() {
                        "(present in flatten)"
                    } else {
                        "(absent in flatten)"
                    }
                ));
                continue;
            }
            Class::ProcSys => {
                if got.is_none() {
                    acceptable.push(format!("[proc-sys] export-only: {path}"));
                }
                continue;
            }
            Class::MutableEtc => {
                let same = match got {
                    Some(t) => t.kind == exp.kind && t.sha256 == exp.sha256,
                    None => false,
                };
                if !same {
                    acceptable.push(format!(
                        "[mutable-etc] {path}: export={} sha={:?} vs flatten={}",
                        kind_str(&exp.kind),
                        exp.sha256.as_deref().map(|s| &s[..12]),
                        got.map(|t| format!(
                            "{} sha={:?}",
                            kind_str(&t.kind),
                            t.sha256.as_deref().map(|s| &s[..12])
                        ))
                        .unwrap_or_else(|| "ABSENT".into()),
                    ));
                }
                continue;
            }
            Class::Compare => {}
        }

        // Device nodes / fifos outside /dev: the flatten skips them by
        // design and records them — acceptable IF recorded.
        if matches!(exp.kind, Kind::Char(..) | Kind::Block(..) | Kind::Fifo) {
            if tree_meta.skipped_specials.contains(path) {
                acceptable.push(format!(
                    "[special-skipped] {} {} (recorded in skipped_specials)",
                    kind_str(&exp.kind),
                    path
                ));
            } else {
                real.push(format!(
                    "SPECIAL {} {} in export but neither in tree nor skipped_specials",
                    kind_str(&exp.kind),
                    path
                ));
            }
            continue;
        }

        let Some(t) = got else {
            real.push(format!(
                "MISSING in flatten: {} {} (mode={:o} size={})",
                kind_str(&exp.kind),
                path,
                exp.mode,
                exp.size
            ));
            continue;
        };

        if t.kind != exp.kind {
            real.push(format!(
                "KIND {path}: export={} flatten={}",
                kind_str(&exp.kind),
                kind_str(&t.kind)
            ));
            continue;
        }
        // Mode: symlink modes are meaningless on Linux (always 0777).
        if exp.kind != Kind::Symlink && t.mode != exp.mode {
            real.push(format!(
                "MODE {path}: export={:o} flatten={:o}",
                exp.mode, t.mode
            ));
        }
        if t.sha256 != exp.sha256 {
            real.push(format!(
                "CONTENT {path}: export sha256={:?} size={} / flatten sha256={:?} size={}",
                exp.sha256, exp.size, t.sha256, t.size
            ));
        }
        if t.link != exp.link {
            real.push(format!(
                "SYMLINK TARGET {path}: export={:?} flatten={:?}",
                exp.link, t.link
            ));
        }

        // uid/gid: export-tar header vs the sidecar record (always),
        // and vs the on-disk tree when ownership was applied.
        match tree_meta.get(path) {
            Some(m) => {
                if (m.uid, m.gid) != (exp.uid, exp.gid) {
                    real.push(format!(
                        "SIDECAR OWNERSHIP {path}: export uid={} gid={} / sidecar uid={} gid={}",
                        exp.uid, exp.gid, m.uid, m.gid
                    ));
                }
                if m.mode != exp.mode && exp.kind != Kind::Symlink {
                    real.push(format!(
                        "SIDECAR MODE {path}: export={:o} sidecar={:o}",
                        exp.mode, m.mode
                    ));
                }
            }
            None => real.push(format!(
                "SIDECAR MISSING record for {path} (present in export + tree)"
            )),
        }
        if ownership_applied && (t.uid, t.gid) != (exp.uid, exp.gid) {
            real.push(format!(
                "TREE OWNERSHIP {path}: export uid={} gid={} / tree uid={} gid={}",
                exp.uid, exp.gid, t.uid, t.gid
            ));
        }

        // xattrs: every export-tar xattr must be on the tree, unless
        // the flatten recorded the refusal.
        for (name, value) in &exp.xattrs {
            match t.xattrs.get(name) {
                Some(v) if v == value => {}
                other => {
                    let skipped = tree_meta
                        .skipped_xattrs
                        .iter()
                        .any(|s| &s.path == path && &s.name == name);
                    let msg = format!(
                        "XATTR {path} {name}: export {}B, flatten {}",
                        value.len(),
                        match other {
                            Some(v) => format!("{}B (different value)", v.len()),
                            None => "absent".into(),
                        }
                    );
                    if skipped {
                        acceptable.push(format!("[xattr-skipped] {msg}"));
                    } else {
                        real.push(msg);
                    }
                }
            }
        }
    }

    // --- 4b. flatten-only paths ---
    for (path, t) in &tree {
        if export.contains_key(path) {
            continue;
        }
        match classify(path) {
            Class::Dev => {
                acceptable.push(format!("[dev] flatten-only: {} {path}", kind_str(&t.kind)))
            }
            Class::ProcSys => {
                acceptable.push(format!("[proc-sys] flatten-only: {path}"));
            }
            Class::MutableEtc => acceptable.push(format!(
                "[mutable-etc] flatten-only: {path} (docker replaced it in the export)"
            )),
            _ => real.push(format!(
                "EXTRA in flatten (not in export): {} {path} mode={:o}",
                kind_str(&t.kind),
                t.mode
            )),
        }
    }

    // --- 4c. hardlink groupings ---
    // For every export hardlink group, the same set of paths must share
    // one inode in the flatten tree, and vice versa.
    let group_of = |m: &BTreeMap<String, Entry>| -> HashMap<String, BTreeSet<String>> {
        let mut g: HashMap<String, BTreeSet<String>> = HashMap::new();
        for (p, e) in m {
            if let Some(rep) = &e.hardlink_rep {
                g.entry(rep.clone()).or_default().insert(p.clone());
            }
        }
        g
    };
    let eg = group_of(&export);
    let tg = group_of(&tree);
    let egroups: BTreeSet<BTreeSet<String>> = eg.into_values().collect();
    let tgroups: BTreeSet<BTreeSet<String>> = tg
        .into_values()
        .map(|s| {
            // drop dev/-class paths so the ignore rules apply here too
            s.into_iter()
                .filter(|p| classify(p) == Class::Compare)
                .collect::<BTreeSet<_>>()
        })
        .filter(|s: &BTreeSet<String>| s.len() >= 2)
        .collect();
    let egroups: BTreeSet<BTreeSet<String>> = egroups
        .into_iter()
        .map(|s| {
            s.into_iter()
                .filter(|p| classify(p) == Class::Compare)
                .collect::<BTreeSet<_>>()
        })
        .filter(|s: &BTreeSet<String>| s.len() >= 2)
        .collect();
    for g in egroups.difference(&tgroups) {
        real.push(format!(
            "HARDLINK GROUP in export but not identically in flatten: {g:?}"
        ));
    }
    for g in tgroups.difference(&egroups) {
        real.push(format!(
            "HARDLINK GROUP in flatten but not identically in export: {g:?}"
        ));
    }

    // --- 5. report ---
    eprintln!(
        "\n-- device/special inventory from the export tar ({}):",
        dev_inventory_export.len()
    );
    for l in &dev_inventory_export {
        eprintln!("   {l}");
    }
    eprintln!("\n-- acceptable deltas ({}):", acceptable.len());
    for l in &acceptable {
        eprintln!("   {l}");
    }
    eprintln!("\n-- REAL DIFFS ({}):", real.len());
    for l in &real {
        eprintln!("   {l}");
    }
    eprintln!("=== END GOLDEN DIFF REPORT: {uri} ===\n");

    assert!(
        real.is_empty(),
        "{} REAL diffs between the flatten and docker export for {uri} (see report above)",
        real.len()
    );
}

/// Full-pipeline smoke on a real image: pull → flatten → inject →
/// mke2fs pack → chunk, twice, asserting the content-derived manifest
/// ref reproduces (the ADR 0036 determinism property on REAL data).
/// Requires a SOURCE_DATE_EPOCH-honoring mke2fs (>= 1.47.1) — pin via
/// ENGRAM_MKE2FS or run inside `nix develop`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual full-pipeline smoke; needs ENGRAM_GOLDEN_DIFF_URI + mke2fs >= 1.47.1"]
async fn golden_full_materialize_smoke() {
    use engram_rootfs_materializer::{InitInjection, Materializer, Transport};

    let uri = env("ENGRAM_GOLDEN_DIFF_URI").expect("set ENGRAM_GOLDEN_DIFF_URI");
    let mke2fs = env("ENGRAM_MKE2FS").unwrap_or_else(|| "mke2fs".into());
    let ver = std::process::Command::new(&mke2fs)
        .arg("-V")
        .output()
        .expect("run mke2fs -V");
    eprintln!(
        "mke2fs: {} — {}",
        mke2fs,
        String::from_utf8_lossy(&ver.stderr)
            .lines()
            .next()
            .unwrap_or("?")
    );

    let store_dir = tempfile::tempdir().unwrap();
    let chunk_store = engram_chunk_store::ChunkStore::new(std::sync::Arc::new(
        engram_storage_local::LocalBlobStorage::new(store_dir.path().to_path_buf()),
    ));
    let scratch = tempfile::tempdir().unwrap();
    let m = Materializer::new(
        OciClient::new(std::sync::Arc::new(AnonymousResolver)),
        InitInjection {
            vsock_port: 1024,
            transport: Transport::Vsock,
            init_script: None,
        },
    );

    let first = m
        .materialize(&uri, platform(), scratch.path(), &chunk_store, None)
        .await
        .expect("first materialize");
    eprintln!(
        "materialized {uri}: manifest={} ext4={}B digest={} env_keys={} workdir={:?}",
        first.disk_manifest,
        first.ext4_size_bytes,
        first.manifest_digest,
        first.oci_defaults.env.len(),
        first.oci_defaults.workdir
    );
    let second = m
        .materialize(&uri, platform(), scratch.path(), &chunk_store, None)
        .await
        .expect("second materialize");
    assert_eq!(
        first.disk_manifest, second.disk_manifest,
        "real-image double-materialize must reproduce the same content-derived ref"
    );
    assert!(
        std::fs::read_dir(scratch.path()).unwrap().next().is_none(),
        "scratch must be scrubbed"
    );
    eprintln!("determinism double-run OK: {}", first.disk_manifest);
}
