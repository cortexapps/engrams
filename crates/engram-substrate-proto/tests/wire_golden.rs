//! Byte-level goldens for the version-3 frames. A client that does not
//! link this crate encodes them by hand, so their bytes must not move.
//!
//! Each golden is a whole frame from [`encode_frame`]: the 4-byte
//! big-endian length, then the bincode body (fixint, little-endian).
//! To write the corpus again after a wire-safe change (a new trailing
//! variant):
//!
//! ```text
//!   cargo test -p engram-substrate-proto --test wire_golden -- --ignored regen_golden
//! ```
//!
//! An existing golden that changes bytes breaks the clients of the
//! version that wrote it.

use std::path::PathBuf;

use engram_substrate_proto::{
    encode_frame, read_frame, ByteRange, FromWriter, Requester, ToWriter,
};

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.bin"))
}

fn read_with_range() -> ToWriter {
    ToWriter::Read {
        hash: [0xAB; 32],
        requester: Requester::Disk,
        range: Some(ByteRange {
            offset: 0x1_0000,
            len: 0x1000,
        }),
    }
}

fn read_whole() -> ToWriter {
    ToWriter::Read {
        hash: [0xCD; 32],
        requester: Requester::Memory,
        range: None,
    }
}

fn filled() -> FromWriter {
    FromWriter::Filled { len: 0x8_0000 }
}

fn assert_golden<T>(name: &str, value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let path = golden_path(name);
    let golden = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden file {}: {e}\nwrite it with: cargo test -p engram-substrate-proto \
             --test wire_golden -- --ignored regen_golden",
            path.display()
        )
    });
    assert_eq!(
        encode_frame(value).unwrap(),
        golden,
        "the frame for `{name}` changed bytes"
    );
    let decoded: T = read_frame(&mut golden.as_slice()).unwrap();
    assert_eq!(
        &decoded, value,
        "the golden for `{name}` decodes to another value"
    );
}

#[test]
fn read_with_a_range_matches_its_golden() {
    assert_golden("read_range", &read_with_range());
}

#[test]
fn read_of_a_whole_chunk_matches_its_golden() {
    assert_golden("read_whole", &read_whole());
}

#[test]
fn filled_matches_its_golden() {
    assert_golden("filled", &filled());
}

#[test]
#[ignore = "writes the golden corpus"]
fn regen_golden() {
    fn write(name: &str, bytes: Vec<u8>) {
        let path = golden_path(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
    }
    write("read_range", encode_frame(&read_with_range()).unwrap());
    write("read_whole", encode_frame(&read_whole()).unwrap());
    write("filled", encode_frame(&filled()).unwrap());
}
