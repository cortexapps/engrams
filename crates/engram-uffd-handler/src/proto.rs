//! Wire format Firecracker uses to hand off the UFFD device to a
//! page-fault handler.
//!
//! Confirmed against upstream
//! `firecracker/src/firecracker/examples/uffd/uffd_utils.rs`:
//!
//! Firecracker calls `recvmsg(2)` on the handler's UDS once at handoff,
//! sending a single fd in `SCM_RIGHTS` (the userfaultfd) and a JSON
//! body in the data portion. The body deserializes to
//! `Vec<GuestRegionUffdMapping>` describing the regions the kernel
//! will fault on. There is no length prefix — the recv buffer is
//! sized 1024 bytes and the actual payload is read in one shot.

use serde::{Deserialize, Serialize};

/// One region of guest memory Firecracker has registered with the
/// kernel as a userfaultfd watcher. The handler maps an address that
/// faults inside `[base_host_virt_addr, base_host_virt_addr + size)`
/// to file offset `offset + (fault_addr - base_host_virt_addr)`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuestRegionUffdMapping {
    /// Virtual address where Firecracker mapped this region in its
    /// own (the guest's) address space. Faulting addresses we read
    /// from the UFFD will fall inside `[base, base+size)`.
    pub base_host_virt_addr: u64,
    /// Region length in bytes.
    pub size: usize,
    /// Offset into `memory.bin` where this region's pages live. We
    /// `mmap` the whole file once, so the handler does pointer math
    /// `mmap_base + offset + intra_region_offset` to find the source.
    pub offset: u64,
    /// Page size for this region in BYTES, despite the misleading
    /// wire-field name. FC v1.10 renamed this from `page_size` to
    /// `page_size_kib` but the *value* is still bytes — upstream's
    /// `HugePageConfig::page_size_kib()` returns 4096 for normal
    /// pages and 2 * 1024 * 1024 for 2 MiB hugepages, both byte
    /// values. We follow the wire name for compatibility but
    /// `runtime.rs` does pointer math in bytes.
    #[serde(rename = "page_size_kib")]
    pub page_size: usize,
}

impl GuestRegionUffdMapping {
    /// `true` if `addr` lies inside this region's virtual range.
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.base_host_virt_addr && addr < self.base_host_virt_addr + self.size as u64
    }
}

/// Maximum size of the JSON handshake body. Matches the upstream
/// example handler's buffer; large enough for dozens of regions.
pub const HANDSHAKE_BUF_BYTES: usize = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_upstream_example_body() {
        // FC v1.10+ wire format: `page_size_kib` field name. The
        // serde alias keeps us tolerant of older `page_size`-named
        // bodies if we ever target an older FC.
        let body = r#"[{"base_host_virt_addr":0,"size":4096,"offset":0,"page_size_kib":4096}]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(body).unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].size, 4096);
        assert_eq!(mappings[0].page_size, 4096);
    }

    #[test]
    fn contains_handles_inclusive_lower_exclusive_upper() {
        let r = GuestRegionUffdMapping {
            base_host_virt_addr: 0x1000,
            size: 0x2000,
            offset: 0,
            page_size: 4096,
        };
        assert!(r.contains(0x1000), "lower bound is inclusive");
        assert!(r.contains(0x2fff), "still inside region at end-1");
        assert!(!r.contains(0x3000), "upper bound is exclusive");
        assert!(!r.contains(0x0fff), "before region");
    }

    #[test]
    fn deserializes_multiple_regions() {
        // Firecracker may split RAM across multiple mappings (e.g.,
        // around the BIOS hole below 1 MiB). The handler must walk
        // the array, not assume a single region.
        let body = r#"[
            {"base_host_virt_addr":0,"size":655360,"offset":0,"page_size_kib":4096},
            {"base_host_virt_addr":1048576,"size":133169152,"offset":655360,"page_size_kib":4096}
        ]"#;
        let mappings: Vec<GuestRegionUffdMapping> = serde_json::from_str(body).unwrap();
        assert_eq!(mappings.len(), 2);
        assert!(mappings[0].contains(0));
        assert!(mappings[1].contains(1048576));
    }
}
