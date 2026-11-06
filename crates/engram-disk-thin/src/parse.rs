//! Parsers for the target parameter strings device-mapper returns and
//! the text the thin tools print. Every function is pure so the formats
//! are pinned by tests that need no pool.

use std::io;

use engram_host_core::disk::{CloneState, Hydration};

/// A block device number: `major:minor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevNo {
    pub major: u32,
    pub minor: u32,
}

impl DevNo {
    pub fn parse(text: &str) -> io::Result<Self> {
        let (major, minor) = text
            .split_once(':')
            .ok_or_else(|| invalid(format!("device number {text:?} is not major:minor")))?;
        Ok(Self {
            major: number(major)?,
            minor: number(minor)?,
        })
    }

    /// The sysfs directory that names the device node.
    pub fn sysfs_path(self) -> String {
        format!("/sys/dev/block/{}:{}", self.major, self.minor)
    }
}

impl std::fmt::Display for DevNo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.major, self.minor)
    }
}

/// `thin-pool` target params:
/// `<metadata> <data> <block sectors> <low water> <n features> ...`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolParams {
    pub metadata_dev: DevNo,
    pub data_dev: DevNo,
    pub block_sectors: u64,
}

pub fn pool_params(params: &str) -> io::Result<PoolParams> {
    let fields: Vec<&str> = params.split_whitespace().collect();
    match fields.as_slice() {
        [metadata, data, block_sectors, ..] => Ok(PoolParams {
            metadata_dev: DevNo::parse(metadata)?,
            data_dev: DevNo::parse(data)?,
            block_sectors: number(block_sectors)?,
        }),
        _ => Err(invalid(format!("not thin-pool params: {params:?}"))),
    }
}

/// `thin` target params: `<pool> <id>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThinParams {
    pub pool: DevNo,
    pub id: u32,
}

pub fn thin_params(params: &str) -> io::Result<ThinParams> {
    let fields: Vec<&str> = params.split_whitespace().collect();
    match fields.as_slice() {
        [pool, id, ..] => Ok(ThinParams {
            pool: DevNo::parse(pool)?,
            id: number(id)?,
        }),
        _ => Err(invalid(format!("not thin params: {params:?}"))),
    }
}

/// `thin-pool` status: `<transaction> <used>/<total> <used>/<total> ...`,
/// metadata first in 4 KiB blocks, then data in pool blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolStatus {
    pub metadata_used_blocks: u64,
    pub metadata_total_blocks: u64,
    pub data_used_blocks: u64,
    pub data_total_blocks: u64,
}

pub const METADATA_BLOCK_BYTES: u64 = 4096;

/// A pool that is not read-write (`ro`, `out_of_data_space`, or a
/// single-word `Fail`) is an error: its numbers do not describe a pool
/// that can take a session.
pub fn pool_status(status: &str) -> io::Result<PoolStatus> {
    let fields: Vec<&str> = status.split_whitespace().collect();
    match fields.as_slice() {
        ["Fail" | "Error"] => Err(invalid(format!("thin-pool is {}", fields[0]))),
        // The mode is the only part of the message: it stays the same
        // from one sample to the next, so a faulted host's capability
        // detail does not change every tick.
        [_, _, _, _, mode, ..] if *mode != "rw" => Err(invalid(format!("thin-pool is {mode}"))),
        [_, metadata, data, _, "rw", ..] => {
            let (metadata_used_blocks, metadata_total_blocks) = fraction(metadata)?;
            let (data_used_blocks, data_total_blocks) = fraction(data)?;
            Ok(PoolStatus {
                metadata_used_blocks,
                metadata_total_blocks,
                data_used_blocks,
                data_total_blocks,
            })
        }
        _ => Err(invalid(format!("not thin-pool status: {status:?}"))),
    }
}

/// `clone` target params: `<metadata> <dest> <source> <region sectors>`,
/// then the feature and core arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloneTable {
    pub metadata_dev: DevNo,
    pub dest_dev: DevNo,
    pub source_dev: DevNo,
    pub region_sectors: u64,
}

impl CloneTable {
    /// The params a table load takes. The kernel reads `no_hydration`
    /// only at load; afterwards the `enable_hydration` message turns it
    /// on.
    pub fn params(&self, hydration: Hydration) -> String {
        let features = if hydration.enabled {
            "0"
        } else {
            "1 no_hydration"
        };
        format!(
            "{} {} {} {} {features} 4 hydration_threshold {} hydration_batch_size {}",
            self.metadata_dev,
            self.dest_dev,
            self.source_dev,
            self.region_sectors,
            hydration.threshold_regions,
            hydration.batch_regions
        )
    }
}

pub fn clone_table(params: &str) -> io::Result<CloneTable> {
    let fields: Vec<&str> = params.split_whitespace().collect();
    match fields.as_slice() {
        [metadata, dest, source, region_sectors, ..] => Ok(CloneTable {
            metadata_dev: DevNo::parse(metadata)?,
            dest_dev: DevNo::parse(dest)?,
            source_dev: DevNo::parse(source)?,
            region_sectors: number(region_sectors)?,
        }),
        _ => Err(invalid(format!("not clone params: {params:?}"))),
    }
}

/// `clone` status: `<metadata block sectors> <used>/<total> <region
/// sectors> <hydrated>/<total regions> <in flight> <n features>
/// <features>... <n core args> <core args>... <mode>`.
///
/// A clone whose metadata is not read-write (`ro`, or a single-word
/// `Fail`) is an error: it cannot record a copy, so its counts no longer
/// move.
pub fn clone_status(status: &str) -> io::Result<CloneState> {
    let fields: Vec<&str> = status.split_whitespace().collect();
    let [_, _, _, regions, _, features, rest @ ..] = fields.as_slice() else {
        return match fields.as_slice() {
            [word] => Err(invalid(format!("clone is {word}"))),
            _ => Err(invalid(format!("not clone status: {status:?}"))),
        };
    };
    let n_features: usize = number(features)?;
    let feature_args = rest
        .get(..n_features)
        .ok_or_else(|| invalid(format!("clone status is short: {status:?}")))?;
    match rest.last() {
        Some(&"rw") => {}
        Some(mode) => return Err(invalid(format!("clone is {mode}"))),
        None => return Err(invalid(format!("not clone status: {status:?}"))),
    }
    let (hydrated_regions, total_regions) = fraction(regions)?;
    Ok(CloneState::Copying {
        hydrated_regions,
        total_regions,
        hydration_enabled: !feature_args.contains(&"no_hydration"),
    })
}

/// The `DEVNAME=` line of a sysfs `uevent` file.
pub fn devname(uevent: &str) -> io::Result<String> {
    uevent
        .lines()
        .find_map(|line| line.strip_prefix("DEVNAME="))
        .map(str::to_string)
        .ok_or_else(|| invalid("uevent has no DEVNAME"))
}

/// Every `dev_id` in `thin_dump --skip-mappings` output.
pub fn device_ids(xml: &str) -> io::Result<Vec<u32>> {
    xml.lines()
        .filter(|line| line.trim_start().starts_with("<device "))
        .map(|line| attribute(line, "dev_id").and_then(number))
        .collect()
}

/// Block indexes that differ between the two thins in `thin_delta`
/// output. `same` ranges are skipped; `different`, `left_only` and
/// `right_only` ranges are expanded.
pub fn delta_blocks(xml: &str) -> io::Result<Vec<u64>> {
    let mut blocks = Vec::new();
    for line in xml.lines() {
        let line = line.trim_start();
        let changed = ["<different ", "<left_only ", "<right_only "]
            .iter()
            .any(|tag| line.starts_with(tag));
        if !changed {
            continue;
        }
        let begin: u64 = attribute(line, "begin").and_then(number)?;
        let length: u64 = attribute(line, "length").and_then(number)?;
        blocks.extend(begin..begin.saturating_add(length));
    }
    blocks.sort_unstable();
    blocks.dedup();
    Ok(blocks)
}

fn attribute<'a>(tag: &'a str, name: &str) -> io::Result<&'a str> {
    let key = format!(" {name}=\"");
    let start = tag
        .find(&key)
        .ok_or_else(|| invalid(format!("no {name} attribute in {tag:?}")))?
        + key.len();
    let end = tag[start..]
        .find('"')
        .ok_or_else(|| invalid(format!("unterminated {name} attribute in {tag:?}")))?;
    Ok(&tag[start..start + end])
}

fn fraction(text: &str) -> io::Result<(u64, u64)> {
    let (used, total) = text
        .split_once('/')
        .ok_or_else(|| invalid(format!("{text:?} is not used/total")))?;
    Ok((number(used)?, number(total)?))
}

fn number<T: std::str::FromStr>(text: &str) -> io::Result<T> {
    text.parse()
        .map_err(|_| invalid(format!("{text:?} is not a number")))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_params_read_devices_and_block_size() {
        let params = pool_params("7:8 7:11 128 32768 0 ").unwrap();
        assert_eq!(
            params,
            PoolParams {
                metadata_dev: DevNo { major: 7, minor: 8 },
                data_dev: DevNo {
                    major: 7,
                    minor: 11
                },
                block_sectors: 128,
            }
        );
        assert!(pool_params("252:0 1").is_err());
    }

    #[test]
    fn thin_params_read_pool_and_id() {
        assert_eq!(
            thin_params("252:0 1").unwrap(),
            ThinParams {
                pool: DevNo {
                    major: 252,
                    minor: 0
                },
                id: 1,
            }
        );
        assert!(thin_params("252:0").is_err());
    }

    #[test]
    fn pool_status_reads_both_fractions() {
        let status =
            pool_status("0 4115/65536 5/65536 - rw discard_passdown queue_if_no_space - 1024")
                .unwrap();
        assert_eq!(
            status,
            PoolStatus {
                metadata_used_blocks: 4115,
                metadata_total_blocks: 65536,
                data_used_blocks: 5,
                data_total_blocks: 65536,
            }
        );
    }

    #[test]
    fn pool_status_rejects_a_pool_that_is_not_read_write() {
        for status in [
            "0 4115/65536 65536/65536 - out_of_data_space discard_passdown error_if_no_space - 1024",
            "0 4115/65536 5/65536 - ro discard_passdown queue_if_no_space - 1024",
            "Fail",
        ] {
            let err = pool_status(status).unwrap_err().to_string();
            assert!(err.starts_with("thin-pool is "), "{status}: {err}");
            assert!(!err.contains('/'), "the message carries no block counts: {err}");
        }
    }

    #[test]
    fn devname_finds_the_node_name() {
        assert_eq!(
            devname("MAJOR=7\nMINOR=8\nDEVNAME=loop8\nDEVTYPE=disk\n").unwrap(),
            "loop8"
        );
        assert!(devname("MAJOR=7\n").is_err());
    }

    #[test]
    fn device_ids_reads_every_device_element() {
        let xml = r#"<superblock uuid="" time="0" transaction="0" flags="0" version="2" data_block_size="128" nr_data_blocks="0">
  <device dev_id="1" mapped_blocks="0" transaction="0" creation_time="0" snap_time="0">
  </device>
  <device dev_id="17" mapped_blocks="3" transaction="0" creation_time="0" snap_time="0">
  </device>
</superblock>"#;
        assert_eq!(device_ids(xml).unwrap(), vec![1, 17]);
    }

    #[test]
    fn delta_blocks_expands_changed_ranges_and_skips_same() {
        let xml = r#"<superblock uuid="" time="0" transaction="0" data_block_size="128" nr_data_blocks="0">
  <diff left="2" right="1">
    <same begin="0" length="100"/>
    <different begin="100" length="2"/>
    <left_only begin="200" length="1"/>
    <right_only begin="300" length="1"/>
    <right_only begin="101" length="1"/>
  </diff>
</superblock>"#;
        assert_eq!(delta_blocks(xml).unwrap(), vec![100, 101, 200, 300]);
        assert!(delta_blocks("<superblock>\n<diff>\n</diff>\n</superblock>")
            .unwrap()
            .is_empty());
    }

    fn dev(major: u32, minor: u32) -> DevNo {
        DevNo { major, minor }
    }

    /// `dmsetup table` of a clone made with hydration off, kernel 6.8.
    const CLONE_TABLE: &str =
        "252:2 252:1 7:4 128 1 no_hydration 4 hydration_threshold 2 hydration_batch_size 3";

    #[test]
    fn clone_params_format_what_the_kernel_reads_back() {
        let table = CloneTable {
            metadata_dev: dev(252, 2),
            dest_dev: dev(252, 1),
            source_dev: dev(7, 4),
            region_sectors: 128,
        };
        let off = Hydration {
            enabled: false,
            threshold_regions: 2,
            batch_regions: 3,
        };
        assert_eq!(table.params(off), CLONE_TABLE);
        assert_eq!(
            table.params(Hydration {
                enabled: true,
                ..off
            }),
            "252:2 252:1 7:4 128 0 4 hydration_threshold 2 hydration_batch_size 3"
        );
        assert_eq!(clone_table(CLONE_TABLE).unwrap(), table);
        assert!(clone_table("252:2 252:1 7:4").is_err());
    }

    #[test]
    fn clone_status_reads_the_region_counts_and_the_hydration_flag() {
        // `dmsetup status` of one clone, kernel 6.8: new, after one guest
        // write, hydrated with hydration on.
        let fresh = clone_status(
            "8 6/1024 128 0/64 0 1 no_hydration 4 hydration_threshold 2 hydration_batch_size 3 rw",
        )
        .unwrap();
        assert_eq!(
            fresh,
            CloneState::Copying {
                hydrated_regions: 0,
                total_regions: 64,
                hydration_enabled: false
            }
        );
        let written = clone_status(
            "8 6/1024 128 1/64 0 1 no_hydration 4 hydration_threshold 2 hydration_batch_size 3 rw",
        )
        .unwrap();
        assert!(matches!(
            written,
            CloneState::Copying {
                hydrated_regions: 1,
                ..
            }
        ));
        let done = clone_status(
            "8 6/1024 128 64/64 0 0 4 hydration_threshold 2 hydration_batch_size 3 rw",
        )
        .unwrap();
        assert_eq!(
            done,
            CloneState::Copying {
                hydrated_regions: 64,
                total_regions: 64,
                hydration_enabled: true
            }
        );
    }

    #[test]
    fn clone_status_rejects_a_clone_that_cannot_record_a_copy() {
        for (status, want) in [
            ("Fail", "clone is Fail"),
            (
                "8 6/1024 128 3/64 0 0 4 hydration_threshold 2 hydration_batch_size 3 ro",
                "clone is ro",
            ),
        ] {
            assert_eq!(clone_status(status).unwrap_err().to_string(), want);
        }
        assert!(clone_status("8 6/1024 128 3/64").is_err());
        assert!(clone_status("8 6/1024 128 3/64 0 2 no_hydration").is_err());
    }
}
