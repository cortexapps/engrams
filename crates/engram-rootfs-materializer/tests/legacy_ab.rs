//! ADR 0093 one-time legacy A/B (LOCAL ONLY — deleted in the same PR
//! by the retirement commit, since mke2fs leaves CI with it): the same
//! fixture layers through the retiring tree+mke2fs pipeline and the
//! streaming pack must produce namespace/content-equal filesystems.
//! Both images are read by the mkext4 verification reader; equality is
//! semantic (paths, kinds, modes, owners, mtimes, targets, bytes,
//! hardlink grouping), not byte-layout.

use std::collections::BTreeMap;

use engram_rootfs_materializer::stream_pack::NamespaceBuilder;
use engram_rootfs_materializer::{ext4, flatten, inject, Ext4Packer, InitInjection, Transport};

struct LayerBuilder {
    tar: tar::Builder<Vec<u8>>,
}

impl LayerBuilder {
    fn new() -> Self {
        Self {
            tar: tar::Builder::new(Vec::new()),
        }
    }
    fn header(mode: u32, uid: u64, gid: u64, size: u64, kind: tar::EntryType) -> tar::Header {
        let mut h = tar::Header::new_gnu();
        h.set_mode(mode);
        h.set_uid(uid);
        h.set_gid(gid);
        h.set_size(size);
        h.set_mtime(946_684_800);
        h.set_entry_type(kind);
        h
    }
    fn file_owned(mut self, path: &str, mode: u32, uid: u64, gid: u64, body: &[u8]) -> Self {
        let mut h = Self::header(mode, uid, gid, body.len() as u64, tar::EntryType::Regular);
        self.tar.append_data(&mut h, path, body).unwrap();
        self
    }
    fn file(self, path: &str, mode: u32, body: &[u8]) -> Self {
        self.file_owned(path, mode, 0, 0, body)
    }
    fn dir(mut self, path: &str, mode: u32) -> Self {
        let mut h = Self::header(mode, 0, 0, 0, tar::EntryType::Directory);
        self.tar.append_data(&mut h, path, &[][..]).unwrap();
        self
    }
    fn symlink(mut self, path: &str, target: &str) -> Self {
        let mut h = Self::header(0o777, 0, 0, 0, tar::EntryType::Symlink);
        self.tar.append_link(&mut h, path, target).unwrap();
        self
    }
    fn hardlink(mut self, path: &str, target: &str) -> Self {
        let mut h = Self::header(0o644, 0, 0, 0, tar::EntryType::Link);
        self.tar.append_link(&mut h, path, target).unwrap();
        self
    }
    fn build(mut self) -> Vec<u8> {
        self.tar.finish().unwrap();
        self.tar.into_inner().unwrap()
    }
}

/// The rich fixture: every semantic feature both pipelines must agree
/// on, including a multi-extent (>16 MiB) file and usrmerge routing.
fn fixture_layers() -> Vec<Vec<u8>> {
    let big: Vec<u8> = (0..20 * 1024 * 1024u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 7) as u8)
        .collect();
    vec![
        LayerBuilder::new()
            .dir("usr", 0o755)
            .dir("usr/bin", 0o755)
            .symlink("bin", "usr/bin")
            .file("usr/bin/busybox", 0o4755, b"#!busybox")
            .hardlink("usr/bin/sh", "usr/bin/busybox")
            .dir("etc", 0o750)
            .file_owned("etc/passwd", 0o644, 0, 0, b"root:x:0:0::/:/bin/sh\n")
            .file_owned("home-file", 0o600, 1000, 1000, b"user data")
            .file("big.bin", 0o644, &big)
            .file("doomed", 0o644, b"remove me")
            .dir("cfg", 0o755)
            .file("cfg/a", 0o644, b"a")
            .file("cfg/b", 0o644, b"b")
            .build(),
        LayerBuilder::new()
            .file("bin/ls", 0o755, b"ls-via-usrmerge") // routes through the symlink
            .file(".wh.doomed", 0o644, b"")
            .dir("cfg", 0o755)
            .file("cfg/upper", 0o644, b"u")
            .file("cfg/.wh..wh..opq", 0o644, b"")
            .file("etc/passwd", 0o600, b"root:x:0:0::/:/bin/bash\n") // overwrite
            .build(),
    ]
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Ent {
    kind: u16,
    mode: u16,
    uid: u32,
    gid: u32,
    mtime: u32,
    size: u64,
    target: Option<Vec<u8>>,
    content_sha: Option<[u8; 32]>,
    link_group: Option<u32>, // inode number normalized below
}

fn walk_image(image: &[u8]) -> BTreeMap<String, Ent> {
    use sha2::Digest;
    let fs = mkext4::reader::Fs::open(image).unwrap();
    {
        let issues = fs.verify().unwrap();
        assert!(issues.is_empty(), "image must verify clean: {issues:?}");
    }
    let mut out = BTreeMap::new();
    let mut stack = vec![(String::new(), 2u32)];
    while let Some((prefix, ino)) = stack.pop() {
        for de in fs.read_dir(ino).unwrap() {
            let name = String::from_utf8_lossy(&de.name).into_owned();
            if name == "." || name == ".." || (prefix.is_empty() && name == "lost+found") {
                continue;
            }
            // The shim is streaming-only by construction here; skip it
            // on both sides (legacy injects it too — keep it in).
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let inode = fs.inode(de.inode).unwrap();
            let kind = inode.mode & 0xF000;
            let ent = Ent {
                kind,
                mode: inode.mode & 0o7777,
                uid: inode.uid as u32,
                gid: inode.gid as u32,
                mtime: inode.mtime,
                size: if kind == 0x8000 { inode.size } else { 0 },
                target: (kind == 0xA000).then(|| fs.symlink_target(de.inode).unwrap()),
                content_sha: (kind == 0x8000).then(|| {
                    let mut h = sha2::Sha256::new();
                    h.update(fs.read_file(de.inode).unwrap());
                    h.finalize().into()
                }),
                link_group: (kind == 0x8000 && inode.links_count > 1).then_some(de.inode),
            };
            out.insert(path.clone(), ent);
            if kind == 0x4000 {
                stack.push((path, de.inode));
            }
        }
    }
    // Normalize hardlink groups: replace raw inode numbers with a
    // stable id per group so different allocation orders compare equal.
    let mut groups: BTreeMap<u32, u32> = BTreeMap::new();
    let paths: Vec<String> = out.keys().cloned().collect();
    for p in paths {
        if let Some(g) = out[&p].link_group {
            let next = groups.len() as u32;
            let id = *groups.entry(g).or_insert(next);
            out.get_mut(&p).unwrap().link_group = Some(id);
        }
    }
    out
}

#[tokio::test]
async fn legacy_and_streaming_pipelines_agree() {
    // Local-only gate: requires mke2fs (nix develop). Skip elsewhere —
    // the whole file is deleted by the retirement commit.
    if std::process::Command::new("mke2fs")
        .arg("-V")
        .output()
        .is_err()
    {
        eprintln!("skipping: mke2fs not on PATH (run under `nix develop`)");
        return;
    }
    let layers = fixture_layers();
    let init = InitInjection {
        vsock_port: 1024,
        transport: Transport::Vsock,
        init_script: None,
    };

    // --- Legacy: tree flatten + inject + mke2fs ---
    let work = tempfile::tempdir().unwrap();
    // mke2fs picks 1024-byte blocks below the "small" threshold; prod
    // images are multi-GiB and always get 4096 (the only size the
    // mkext4 reader speaks). Pin 4096 for the fixture-sized image so
    // the legacy side matches prod parameters.
    let conf = work.path().join("mke2fs.conf");
    std::fs::write(
        &conf,
        "[defaults]\n\
         \tbase_features = sparse_super,large_file,filetype,dir_index,ext_attr\n\
         \tblocksize = 4096\n\
         \tinode_size = 256\n\
         \tinode_ratio = 16384\n\
         [fs_types]\n\
         \text4 = {\n\
         \t\tfeatures = has_journal,extent,huge_file,flex_bg,metadata_csum,extra_isize\n\
         \t}\n",
    )
    .unwrap();
    std::env::set_var("MKE2FS_CONFIG", &conf);
    let rootfs = work.path().join("rootfs");
    std::fs::create_dir_all(&rootfs).unwrap();
    let mut meta = flatten::TreeMetadata::default();
    let mut flattener =
        flatten::Flattener::new(&rootfs, flatten::default_write_concurrency()).unwrap();
    for l in &layers {
        flattener.apply_layer(&mut meta, &l[..]).unwrap();
    }
    let stats = flattener.finish(&mut meta).unwrap();
    inject::inject_init(&rootfs, &init).await.unwrap();
    let image_path = work.path().join("legacy.ext4");
    // ≥512 MiB keeps mke2fs on its default (prod-shaped) profile:
    // 4096-byte blocks, 32768 blocks/group — the geometry the mkext4
    // reader speaks and the one every real image gets.
    let size = ext4::recommended_size(stats.tree_bytes_hint).max(512 * 1024 * 1024);
    ext4::Mke2fsPacker::default()
        .pack(&rootfs, &image_path, size)
        .await
        .unwrap();
    let legacy_image = std::fs::read(&image_path).unwrap();

    // --- Streaming ---
    let mut ns = NamespaceBuilder::new();
    for l in &layers {
        ns.declare_layer(&l[..]).unwrap();
    }
    let (rel, mode, body) = inject::rendered_init_shim(&init).await.unwrap();
    ns.declare_synthetic(&rel, mode, body).unwrap();
    let sealed = ns.seal().unwrap();
    let mut sink = mkext4::sink::VecSink::default();
    let mut w = sealed.begin(&mut sink).unwrap();
    for (i, l) in layers.iter().enumerate() {
        sealed.fill_layer(&mut w, i, &l[..]).unwrap();
    }
    sealed.fill_synthetic(&mut w).unwrap();
    engram_rootfs_materializer::stream_pack::SealedImage::finish_writer(w).unwrap();

    // --- Compare (legacy ran unprivileged: ownership lives in the
    // sidecar, not the tree — mask uid/gid on the legacy side by
    // OVERLAYING the sidecar's records, which is exactly what the
    // host RPC does when it runs as root) ---
    let mut legacy = walk_image(&legacy_image);
    for (path, ent) in legacy.iter_mut() {
        match meta.get(path) {
            Some(m) => {
                ent.uid = m.uid as u32;
                ent.gid = m.gid as u32;
            }
            // No sidecar record = engrams-created (inject's sbin,
            // implied parents): root-owned in prod where the host RPC
            // runs as root; uid 501 here is the unprivileged-test
            // artifact.
            None => {
                ent.uid = 0;
                ent.gid = 0;
            }
        }
        // macOS-only artifact: symlink(2) honors umask here, Linux
        // symlinks are always 0o777 (no lchmod). Prod ran on Linux.
        if ent.kind == 0xA000 {
            ent.mode = 0o777;
        }
    }
    let streaming = walk_image(&sink.buf);

    let legacy_paths: Vec<_> = legacy.keys().collect();
    let streaming_paths: Vec<_> = streaming.keys().collect();
    assert_eq!(legacy_paths, streaming_paths, "namespace must match");
    for (path, l) in &legacy {
        let s = &streaming[path];
        assert_eq!(l, s, "mismatch at {path}");
    }
}
