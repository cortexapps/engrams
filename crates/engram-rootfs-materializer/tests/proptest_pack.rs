//! ADR 0099 H3 — the mkext4 flagship property.
//!
//! Arbitrary host file trees (nested dirs bounded ~4 deep, empty files,
//! small + multi-block files, varied modes, symlinks) are packed through
//! the crate's REAL packing path (`stream_pack::pack_tree`, the `mke2fs -d`
//! replacement of ADR 0080 §D / ADR 0093), then:
//!
//!   - opened with `mkext4::reader::Fs::open` and `verify()` reports ZERO
//!     structural issues (checksums, htree placement, extent ordering,
//!     bitmap padding);
//!   - every file reads back byte-identical with its mode, every symlink
//!     resolves to the right target, every dir is present with its mode;
//!   - packing the SAME tree twice produces BYTE-IDENTICAL images —
//!     determinism is ADR 0093's core claim and precisely why mkext4 is
//!     pinned EXACT (`=0.0.3`): its on-disk byte layout is contractual.
//!
//! Caps are LOW (24 cases — tree packing is far heavier than a manifest
//! property) and the big-file case is ONE deterministic test, not a
//! property, so the suite respects nextest's 3-minute slow-timeout. A
//! counterexample landing INSIDE mkext4 (not this materializer) cannot be
//! fixed here — the pin is exact; the path is an upstream release + pin
//! bump, and the shrunk regression seed under `proptest-regressions/`
//! documents it (ADR 0099 H3 caveat).

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use engram_rootfs_materializer::pack_tree;
use mkext4::reader::Fs;
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Node {
    File {
        mode: u16,
        contents: Vec<u8>,
    },
    Symlink {
        target: String,
    },
    Dir {
        mode: u16,
        entries: Vec<(String, Node)>,
    },
}

/// What we expect to read back for one packed path.
enum Expect {
    File { mode: u16, contents: Vec<u8> },
    Symlink { target: String },
    Dir { mode: u16 },
}

fn arb_name() -> impl Strategy<Value = String> {
    // No `/`, no `.`/`..`, non-empty — a legal single path component.
    "[a-z0-9_]{1,10}"
}

fn arb_file_mode() -> impl Strategy<Value = u16> {
    prop::sample::select(vec![0o644u16, 0o600, 0o755, 0o444, 0o640, 0o700, 0o777])
}

/// Directory modes keep owner rwx so `pack_tree`'s post-write sorted walk
/// can still traverse+stat children when we run as the owning (non-root)
/// test user — a 0o444 dir would make the walk's `symlink_metadata` fail,
/// which is a property of POSIX traversal, not of the packer.
fn arb_dir_mode() -> impl Strategy<Value = u16> {
    prop::sample::select(vec![0o755u16, 0o700, 0o750, 0o775, 0o777])
}

fn arb_contents() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => Just(Vec::new()),                                  // empty file
        6 => prop::collection::vec(any::<u8>(), 1..256),        // small (< 1 block)
        2 => prop::collection::vec(any::<u8>(), 4000..9000),    // multi-block (> 4 KiB)
    ]
}

fn arb_symlink_target() -> impl Strategy<Value = String> {
    // Spans fast (in-inode, < 60 bytes) and slow (block) symlink storage.
    "[a-z0-9_/.]{1,80}"
}

fn arb_node() -> impl Strategy<Value = Node> {
    let leaf = prop_oneof![
        (arb_file_mode(), arb_contents())
            .prop_map(|(mode, contents)| Node::File { mode, contents }),
        arb_symlink_target().prop_map(|target| Node::Symlink { target }),
    ];
    // depth <= 4, ~40 total nodes, up to 6 children per interior dir.
    leaf.prop_recursive(4, 40, 6, |inner| {
        (
            arb_dir_mode(),
            prop::collection::vec((arb_name(), inner), 0..6),
        )
            .prop_map(|(mode, entries)| Node::Dir { mode, entries })
    })
}

fn arb_tree() -> impl Strategy<Value = Vec<(String, Node)>> {
    prop::collection::vec((arb_name(), arb_node()), 0..8)
}

/// Realize a generated tree on disk, recording the read-back expectation
/// for every entry. Name collisions within a directory are skipped (the
/// first writer wins) so the fixture stays a valid filesystem.
fn write_entries(
    disk_dir: &Path,
    fs_prefix: &str,
    entries: &[(String, Node)],
    out: &mut Vec<(String, Expect)>,
) {
    let mut seen = HashSet::new();
    for (name, node) in entries {
        if !seen.insert(name.clone()) {
            continue;
        }
        let disk = disk_dir.join(name);
        let fs_path = format!("{fs_prefix}/{name}");
        match node {
            Node::File { mode, contents } => {
                std::fs::write(&disk, contents).unwrap();
                std::fs::set_permissions(&disk, std::fs::Permissions::from_mode(*mode as u32))
                    .unwrap();
                out.push((
                    fs_path,
                    Expect::File {
                        mode: *mode,
                        contents: contents.clone(),
                    },
                ));
            }
            Node::Symlink { target } => {
                std::os::unix::fs::symlink(target, &disk).unwrap();
                out.push((
                    fs_path,
                    Expect::Symlink {
                        target: target.clone(),
                    },
                ));
            }
            Node::Dir { mode, entries } => {
                std::fs::create_dir(&disk).unwrap();
                // Children first, THEN clamp the dir mode — a restrictive
                // (but still owner-traversable) mode must not block writing.
                write_entries(&disk, &fs_path, entries, out);
                std::fs::set_permissions(&disk, std::fs::Permissions::from_mode(*mode as u32))
                    .unwrap();
                out.push((fs_path, Expect::Dir { mode: *mode }));
            }
        }
    }
}

/// Pack `root` to `img_path`, open + verify the image, and assert every
/// recorded expectation reads back exactly. Returns the packed bytes.
fn pack_verify_readback(root: &Path, img_path: &Path, expects: &[(String, Expect)]) -> Vec<u8> {
    pack_tree(root, img_path).expect("pack_tree");
    let bytes = std::fs::read(img_path).unwrap();
    let fs = Fs::open(&bytes[..]).expect("Fs::open on the packed image");
    let issues = fs.verify().expect("verify");
    assert!(
        issues.is_empty(),
        "packed image must verify clean: {issues:?}"
    );

    for (path, exp) in expects {
        let ino = fs
            .resolve(path)
            .unwrap_or_else(|e| panic!("resolve {path}: {e}"));
        match exp {
            Expect::File { mode, contents } => {
                let got = fs.read_file(ino).unwrap();
                assert_eq!(&got, contents, "file {path} content mismatch");
                assert_eq!(
                    fs.inode(ino).unwrap().mode & 0o7777,
                    *mode,
                    "file {path} mode mismatch",
                );
            }
            Expect::Symlink { target } => {
                let got = fs.symlink_target(ino).unwrap();
                assert_eq!(got, target.as_bytes(), "symlink {path} target mismatch");
            }
            Expect::Dir { mode } => {
                assert_eq!(
                    fs.inode(ino).unwrap().mode & 0o7777,
                    *mode,
                    "dir {path} mode mismatch",
                );
            }
        }
    }
    bytes
}

proptest! {
    // Low case count: each case realizes a tree on disk, packs it TWICE,
    // and reads every entry back. tree-packing is heavier than a manifest
    // property, and the 3-minute nextest slow-timeout is the budget.
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_tree_packs_verifies_reads_back_and_repacks_byte_identical(
        tree in arb_tree(),
    ) {
        let root = tempfile::tempdir().unwrap();
        // Image files live OUTSIDE the packed root (else pack_tree walks them).
        let out = tempfile::tempdir().unwrap();

        let mut expects = Vec::new();
        write_entries(root.path(), "", &tree, &mut expects);

        let img1 = pack_verify_readback(root.path(), &out.path().join("a.ext4"), &expects);

        // Determinism (ADR 0093): the SAME tree packs byte-identical.
        pack_tree(root.path(), &out.path().join("b.ext4")).unwrap();
        let img2 = std::fs::read(out.path().join("b.ext4")).unwrap();
        prop_assert_eq!(img1.len(), img2.len(), "repack image length differs");
        prop_assert!(img1 == img2, "repack is not byte-identical");
    }
}

/// One deterministic multi-MiB case (NOT a property, to respect the
/// slow-timeout): a ~12 MiB file exercises multi-extent mapping and reads
/// back byte-identical, alongside a nested dir + symlink, and repacks
/// byte-identical. True block-GROUP crossing (> 128 MiB) is a throughput
/// exercise measured ad hoc on the dev VM, never in a CI test (ADR 0099 H3
/// / the FC-lane "size a test to the property, not to realism" rule).
#[test]
fn multi_megabyte_file_round_trips_and_repacks_byte_identical() {
    let root = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();

    let n = 12 * 1024 * 1024usize;
    // Byte-varied (not all-zero, which would elide to sparse) so the file
    // is a real multi-extent body.
    let big: Vec<u8> = (0..n)
        .map(|i| i.wrapping_mul(2_654_435_761) as u8)
        .collect();

    std::fs::create_dir(root.path().join("d")).unwrap();
    std::fs::write(root.path().join("d/big.bin"), &big).unwrap();
    std::fs::set_permissions(
        root.path().join("d/big.bin"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    std::os::unix::fs::symlink("d/big.bin", root.path().join("link")).unwrap();

    let img_path = out.path().join("big.ext4");
    pack_tree(root.path(), &img_path).unwrap();
    let bytes = std::fs::read(&img_path).unwrap();
    let fs = Fs::open(&bytes[..]).unwrap();
    assert!(
        fs.verify().unwrap().is_empty(),
        "multi-MiB image must verify clean",
    );

    let ino = fs.resolve("/d/big.bin").unwrap();
    assert_eq!(
        fs.read_file(ino).unwrap(),
        big,
        "12 MiB file must read back byte-identical",
    );
    let link = fs.resolve("/link").unwrap();
    assert_eq!(fs.symlink_target(link).unwrap(), b"d/big.bin");

    let img2_path = out.path().join("big2.ext4");
    pack_tree(root.path(), &img2_path).unwrap();
    assert_eq!(
        bytes,
        std::fs::read(&img2_path).unwrap(),
        "12 MiB repack must be byte-identical (ADR 0093 determinism)",
    );
}
