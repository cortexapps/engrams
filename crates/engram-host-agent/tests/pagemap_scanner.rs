//! ADR 0045 C2: the Rust dirty-map scanner + page-server read path
//! against a REAL substrate-shaped process.
//!
//! The `pagemap_probe.c` spike (a `substrate_kernel_capabilities` CI
//! gate) proved the kernel behaviors; this test proves OUR production
//! code on top of them: `dirty_map::{guest_vmas, scan_dirty_chunks}`
//! and `migrate_peer::{read_guest_range, classify_served_chunk}` run
//! from the parent's seat (the host-agent's — we spawn the child, so
//! YAMA permits pagemap reads and `process_vm_readv`).
//!
//! The child fixture (`fixtures/pagemap_scanner_child.c`) maps a tmpfs
//! base file MAP_PRIVATE + UFFD MISSING|MINOR and touches 8 pages into
//! the four classified states; chunk_size = 2 pages → expected seal
//! bitmap `[1, 0, 1, 1]` (see the fixture header for the layout).
//!
//! `#[ignore]`d like the FC suite (needs Linux + unprivileged UFFD +
//! gcc + a tmpfs at /dev/shm); wired into ci.yml's `test-firecracker`
//! job next to the host-agent migration tests.

#![cfg(target_os = "linux")]

use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use engram_chunk_store::ChunkHash;
use engram_host_agent::dirty_map::{guest_vmas, scan_dirty_chunks, GuestVma};
use engram_host_agent::migrate_peer::{classify_served_chunk, read_guest_range, ServedChunk};

const PAGE: u64 = 4096;
const NPAGES: u64 = 8;
const CHUNK: u64 = 2 * PAGE;
const BASE_BYTE: u8 = 0xB5;
const COPY_BYTE: u8 = 0xC0;
const COW_BYTE: u8 = 0xCB;

fn compile_fixture() -> PathBuf {
    let src =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pagemap_scanner_child.c");
    let out_dir =
        std::env::temp_dir().join(format!("engram-pagemap-scanner-{}", std::process::id()));
    std::fs::create_dir_all(&out_dir).expect("create tmpdir");
    let bin = out_dir.join("pagemap_scanner_child");
    let cc = Command::new("gcc")
        .args(["-O2", "-pthread", "-o"])
        .arg(&bin)
        .arg(&src)
        .output()
        .expect("spawn gcc (required on FC test runners)");
    assert!(
        cc.status.success(),
        "gcc failed:\n{}",
        String::from_utf8_lossy(&cc.stderr)
    );
    bin
}

#[test]
#[ignore = "needs Linux + unprivileged userfaultfd + gcc + /dev/shm"]
fn scanner_classifies_and_page_server_reads_a_real_substrate_child() {
    let bin = compile_fixture();
    // UFFD MINOR is shmem-only: the base file must live on tmpfs.
    let base_path = PathBuf::from(format!(
        "/dev/shm/engram-pagemap-scanner-{}.base",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&base_path);

    let mut child = Command::new(&bin)
        .arg(&base_path)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn fixture child");
    let pid = child.id();

    // Wait for "READY <map_base_hex>" (or a FAIL line).
    let stdout = child.stdout.take().expect("child stdout");
    let mut lines = std::io::BufReader::new(stdout).lines();
    let ready = lines
        .next()
        .expect("child printed nothing")
        .expect("read child stdout");
    assert!(
        ready.starts_with("READY "),
        "fixture child failed before READY: {ready}"
    );

    let result = std::panic::catch_unwind(|| {
        // ---- guest_vmas: finds exactly the base-file mapping. ----
        let vmas = guest_vmas(pid, &base_path).expect("read child maps");
        assert_eq!(
            vmas.len(),
            1,
            "expected exactly one base-backed VMA: {vmas:?}"
        );
        let map_base = u64::from_str_radix(ready.trim_start_matches("READY ").trim(), 16)
            .expect("parse map base");
        assert_eq!(
            vmas[0],
            GuestVma {
                start: map_base,
                end: map_base + NPAGES * PAGE,
                file_offset: 0
            }
        );

        // ---- scan_dirty_chunks: the chunk classification. ----
        let seal = scan_dirty_chunks(pid, &vmas, CHUNK, NPAGES * PAGE).expect("pagemap scan");
        assert_eq!(seal.chunk_count, 4);
        let got: Vec<bool> = (0..4).map(|i| seal.get(i)).collect();
        assert_eq!(
            got,
            vec![true, false, true, true],
            "seal bitmap mismatch (COPY / clean / COW / zero-COPY layout)"
        );
        assert_eq!(seal.count_ones(), 3);

        // ---- read_guest_range: serve each sealed chunk. ----
        // Chunk 0: page 0 COPY-installed; page 1 readv-faults through
        // the child's OWN handler (MISSING → COPY_BYTE). Peer-
        // authoritative content ⇒ Page.
        let c0 = read_guest_range(pid, &vmas, 0, CHUNK as usize).expect("read chunk 0");
        assert!(c0.iter().all(|b| *b == COPY_BYTE), "chunk 0 bytes");
        match classify_served_chunk(c0.clone(), None) {
            ServedChunk::Page(bytes, hash) => {
                assert_eq!(bytes, c0);
                assert_eq!(hash, ChunkHash::of(&c0));
            }
            other => panic!("chunk 0 should serve Page, got {other:?}"),
        }

        // Chunk 2: page 4 COW-written + page 5 readv-faults MINOR →
        // CONTINUE → base bytes.
        let c2 = read_guest_range(pid, &vmas, 2 * CHUNK, CHUNK as usize).expect("read chunk 2");
        // The COW write touched exactly one byte; the rest of page 4 is
        // the CONTINUE'd base content carried into the private copy.
        assert_eq!(c2[0], COW_BYTE, "the COW-written byte");
        assert!(c2[1..].iter().all(|b| *b == BASE_BYTE), "rest of chunk 2");
        // The AltSource demote: when the durable manifest already holds
        // exactly these bytes, the server answers AltSource.
        let live_hash = ChunkHash::of(&c2);
        match classify_served_chunk(c2, Some(&live_hash)) {
            ServedChunk::AltSource(h) => assert_eq!(h, live_hash),
            other => panic!("matching durable hash should demote to AltSource, got {other:?}"),
        }

        // Chunk 3: zero-COPY page + readv-faulted zero page ⇒ ZeroChunk
        // (never 512 KiB of zeros on the wire).
        let c3 = read_guest_range(pid, &vmas, 3 * CHUNK, CHUNK as usize).expect("read chunk 3");
        assert!(c3.iter().all(|b| *b == 0), "chunk 3 must be all-zero");
        assert_eq!(classify_served_chunk(c3, None), ServedChunk::Zero);

        // Uncovered range: loud error, not silence (scan/protocol bug).
        let err = read_guest_range(pid, &vmas, NPAGES * PAGE, CHUNK as usize)
            .expect_err("offset past the VMA must error");
        assert!(err.to_string().contains("not covered"));
    });

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&base_path);
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}
