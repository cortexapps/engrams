//! Device-mapper through its ioctl interface, and the two thin tools
//! that read pool metadata the kernel exposes no interface for.
//!
//! The wire format is `struct dm_ioctl` from `linux/dm-ioctl.h`: a
//! 312-byte header followed by a command-specific payload. The layout
//! has been ABI-stable at interface version 4 since 2.6; the constants
//! below were read from the header and checked with a compiled probe.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::parse::{self, DevNo};

const HEADER_LEN: usize = 0x138;
const DM_IOCTL: u32 = 0xfd;
const DM_DEV_CREATE_CMD: u32 = 3;
const DM_DEV_REMOVE_CMD: u32 = 4;
const DM_DEV_SUSPEND_CMD: u32 = 6;
const DM_DEV_STATUS_CMD: u32 = 7;
const DM_TABLE_LOAD_CMD: u32 = 9;
const DM_TABLE_STATUS_CMD: u32 = 12;
const DM_TARGET_MSG_CMD: u32 = 14;

const DM_SUSPEND_FLAG: u32 = 1 << 1;
const DM_STATUS_TABLE_FLAG: u32 = 1 << 4;
const DM_BUFFER_FULL_FLAG: u32 = 1 << 8;

const OFF_DATA_SIZE: usize = 0x0c;
const OFF_DATA_START: usize = 0x10;
const OFF_TARGET_COUNT: usize = 0x14;
const OFF_FLAGS: usize = 0x1c;
const OFF_DEV: usize = 0x28;
const OFF_NAME: usize = 0x30;
const NAME_LEN: usize = 128;

const TARGET_SPEC_LEN: usize = 0x28;
const SPEC_OFF_LENGTH: usize = 0x08;
const SPEC_OFF_NEXT: usize = 0x14;
const SPEC_OFF_TYPE: usize = 0x18;
const TYPE_LEN: usize = 16;

/// `_IOWR(DM_IOCTL, cmd, struct dm_ioctl)`.
const fn request(cmd: u32) -> libc::Ioctl {
    (3 << 30 | (HEADER_LEN as u32) << 16 | DM_IOCTL << 8 | cmd) as libc::Ioctl
}

/// One target line of a table or status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub length_sectors: u64,
    pub kind: String,
    pub params: String,
}

struct Request {
    buf: Vec<u8>,
}

impl Request {
    fn new(name: &str, flags: u32) -> io::Result<Self> {
        if name.len() >= NAME_LEN || name.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("device name {name:?} is too long for device-mapper"),
            ));
        }
        let mut buf = vec![0u8; HEADER_LEN];
        for (i, part) in [4u32, 0, 0].into_iter().enumerate() {
            buf[i * 4..i * 4 + 4].copy_from_slice(&part.to_ne_bytes());
        }
        buf[OFF_DATA_START..OFF_DATA_START + 4].copy_from_slice(&(HEADER_LEN as u32).to_ne_bytes());
        buf[OFF_FLAGS..OFF_FLAGS + 4].copy_from_slice(&flags.to_ne_bytes());
        buf[OFF_NAME..OFF_NAME + name.len()].copy_from_slice(name.as_bytes());
        Ok(Self { buf })
    }

    /// Append a `dm_target_spec` and its parameter string.
    fn push_target(&mut self, length_sectors: u64, kind: &str, params: &str) {
        let start = self.buf.len();
        self.buf.resize(start + TARGET_SPEC_LEN, 0);
        self.buf[start + SPEC_OFF_LENGTH..start + SPEC_OFF_LENGTH + 8]
            .copy_from_slice(&length_sectors.to_ne_bytes());
        self.buf[start + SPEC_OFF_TYPE..start + SPEC_OFF_TYPE + kind.len()]
            .copy_from_slice(kind.as_bytes());
        self.buf.extend_from_slice(params.as_bytes());
        self.buf.push(0);
        let padded = self.buf.len().next_multiple_of(8);
        self.buf.resize(padded, 0);
        let next = (padded - start) as u32;
        self.buf[start + SPEC_OFF_NEXT..start + SPEC_OFF_NEXT + 4]
            .copy_from_slice(&next.to_ne_bytes());
        let count = u32::from_ne_bytes(self.field(OFF_TARGET_COUNT)) + 1;
        self.buf[OFF_TARGET_COUNT..OFF_TARGET_COUNT + 4].copy_from_slice(&count.to_ne_bytes());
    }

    /// Append a `dm_target_msg` for sector 0.
    fn push_message(&mut self, text: &str) {
        self.buf.extend_from_slice(&0u64.to_ne_bytes());
        self.buf.extend_from_slice(text.as_bytes());
        self.buf.push(0);
    }

    fn reserve(&mut self, total: usize) {
        if self.buf.len() < total {
            self.buf.resize(total, 0);
        }
    }

    fn field(&self, offset: usize) -> [u8; 4] {
        self.buf[offset..offset + 4].try_into().unwrap()
    }

    fn flags(&self) -> u32 {
        u32::from_ne_bytes(self.field(OFF_FLAGS))
    }

    fn dev(&self) -> DevNo {
        let dev = u64::from_ne_bytes(self.buf[OFF_DEV..OFF_DEV + 8].try_into().unwrap());
        DevNo {
            major: ((dev >> 8) & 0xfff) as u32,
            minor: ((dev & 0xff) | ((dev >> 12) & 0xfff00)) as u32,
        }
    }

    /// The targets the kernel wrote after a status request.
    fn targets(&self) -> io::Result<Vec<Target>> {
        let count = u32::from_ne_bytes(self.field(OFF_TARGET_COUNT)) as usize;
        let mut at = u32::from_ne_bytes(self.field(OFF_DATA_START)) as usize;
        let mut targets = Vec::with_capacity(count);
        for _ in 0..count {
            let spec = self
                .buf
                .get(at..at + TARGET_SPEC_LEN)
                .ok_or_else(|| invalid("target spec past the end of the reply"))?;
            let length_sectors = u64::from_ne_bytes(
                spec[SPEC_OFF_LENGTH..SPEC_OFF_LENGTH + 8]
                    .try_into()
                    .unwrap(),
            );
            let next =
                u32::from_ne_bytes(spec[SPEC_OFF_NEXT..SPEC_OFF_NEXT + 4].try_into().unwrap());
            let kind = c_str(&spec[SPEC_OFF_TYPE..SPEC_OFF_TYPE + TYPE_LEN]);
            let params = self
                .buf
                .get(at + TARGET_SPEC_LEN..)
                .map(c_str)
                .ok_or_else(|| invalid("target params past the end of the reply"))?;
            targets.push(Target {
                length_sectors,
                kind,
                params,
            });
            at += next as usize;
        }
        Ok(targets)
    }

    /// Issue `cmd` on a blocking thread; the kernel reads and rewrites
    /// the buffer in place.
    async fn ioctl(mut self, control: Arc<File>, cmd: u32) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || {
            let size = self.buf.len() as u32;
            self.buf[OFF_DATA_SIZE..OFF_DATA_SIZE + 4].copy_from_slice(&size.to_ne_bytes());
            ioctl_in_place(&control, request(cmd), &mut self.buf)?;
            Ok(self)
        })
        .await
        .map_err(io::Error::other)?
    }
}

/// SAFETY: `buf` is at least `HEADER_LEN` bytes, its `data_size` field
/// equals its length, and the kernel writes only within `data_size`
/// (`linux/dm-ioctl.h`, `struct dm_ioctl::data_size`). The pointer and
/// the file descriptor outlive the call.
#[allow(unsafe_code)]
fn ioctl_in_place(control: &File, req: libc::Ioctl, buf: &mut [u8]) -> io::Result<()> {
    debug_assert!(buf.len() >= HEADER_LEN);
    let rc = unsafe { libc::ioctl(control.as_raw_fd(), req, buf.as_mut_ptr()) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn c_str(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// A handle on `/dev/mapper/control`.
#[derive(Clone)]
pub struct Dm {
    control: Arc<File>,
}

impl Dm {
    pub fn open() -> io::Result<Self> {
        let control = File::options()
            .read(true)
            .write(true)
            .open("/dev/mapper/control")?;
        Ok(Self {
            control: Arc::new(control),
        })
    }

    async fn call(&self, op: &str, name: &str, req: Request, cmd: u32) -> io::Result<Request> {
        req.ioctl(self.control.clone(), cmd).await.map_err(|e| {
            let kind = match e.raw_os_error() {
                Some(libc::ENXIO) => io::ErrorKind::NotFound,
                Some(libc::EEXIST) => io::ErrorKind::AlreadyExists,
                _ => e.kind(),
            };
            io::Error::new(kind, format!("dm {op} {name}: {e}"))
        })
    }

    /// Create a device, load its single target and resume it.
    pub async fn create(
        &self,
        name: &str,
        length_sectors: u64,
        kind: &str,
        params: &str,
    ) -> io::Result<()> {
        self.call("create", name, Request::new(name, 0)?, DM_DEV_CREATE_CMD)
            .await
            .map_err(|e| {
                if e.raw_os_error() == Some(libc::EBUSY) {
                    io::Error::new(io::ErrorKind::AlreadyExists, e)
                } else {
                    e
                }
            })?;
        let mut load = Request::new(name, 0)?;
        load.push_target(length_sectors, kind, params);
        let loaded = self.call("load", name, load, DM_TABLE_LOAD_CMD).await;
        if let Err(e) = loaded {
            let _ = self.remove(name).await;
            return Err(e);
        }
        self.resume(name).await
    }

    pub async fn remove(&self, name: &str) -> io::Result<()> {
        self.call("remove", name, Request::new(name, 0)?, DM_DEV_REMOVE_CMD)
            .await
            .map(drop)
    }

    /// Flush in-flight I/O and hold new I/O until `resume`.
    pub async fn suspend(&self, name: &str) -> io::Result<()> {
        self.call(
            "suspend",
            name,
            Request::new(name, DM_SUSPEND_FLAG)?,
            DM_DEV_SUSPEND_CMD,
        )
        .await
        .map(drop)
    }

    pub async fn resume(&self, name: &str) -> io::Result<()> {
        self.call("resume", name, Request::new(name, 0)?, DM_DEV_SUSPEND_CMD)
            .await
            .map(drop)
    }

    pub async fn message(&self, name: &str, text: &str) -> io::Result<()> {
        let mut req = Request::new(name, 0)?;
        req.push_message(text);
        self.call("message", name, req, DM_TARGET_MSG_CMD)
            .await
            .map(drop)
    }

    pub async fn devno(&self, name: &str) -> io::Result<DevNo> {
        let reply = self
            .call("status", name, Request::new(name, 0)?, DM_DEV_STATUS_CMD)
            .await?;
        Ok(reply.dev())
    }

    /// The active table's target lines.
    pub async fn table(&self, name: &str) -> io::Result<Vec<Target>> {
        self.targets(name, DM_STATUS_TABLE_FLAG).await
    }

    /// The active table's status lines.
    pub async fn status(&self, name: &str) -> io::Result<Vec<Target>> {
        self.targets(name, 0).await
    }

    async fn targets(&self, name: &str, flags: u32) -> io::Result<Vec<Target>> {
        let mut room = 4096;
        loop {
            let mut req = Request::new(name, flags)?;
            req.reserve(room);
            let reply = self.call("table", name, req, DM_TABLE_STATUS_CMD).await?;
            if reply.flags() & DM_BUFFER_FULL_FLAG == 0 {
                return reply.targets();
            }
            room *= 2;
        }
    }
}

/// The device node for a `major:minor`, from sysfs.
pub async fn device_path(devno: DevNo) -> io::Result<PathBuf> {
    let uevent = tokio::fs::read_to_string(format!("{}/uevent", devno.sysfs_path())).await?;
    Ok(PathBuf::from("/dev").join(parse::devname(&uevent)?))
}

/// Names of the device-mapper devices stacked on `pool`.
pub async fn devices_on(pool: DevNo) -> io::Result<Vec<String>> {
    let pool_link = tokio::fs::read_link(pool.sysfs_path()).await?;
    let pool_dm = pool_link
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| invalid("pool sysfs link has no name"))?;
    let mut names = Vec::new();
    let mut entries = tokio::fs::read_dir("/sys/block").await?;
    while let Some(entry) = entries.next_entry().await? {
        let dir = entry.path();
        if !entry.file_name().to_string_lossy().starts_with("dm-") {
            continue;
        }
        if tokio::fs::metadata(dir.join("slaves").join(&pool_dm))
            .await
            .is_err()
        {
            continue;
        }
        let name = tokio::fs::read_to_string(dir.join("dm/name")).await?;
        names.push(name.trim_end().to_string());
    }
    Ok(names)
}

/// Create a block device node, replacing one that points elsewhere.
#[allow(unsafe_code)]
pub fn make_node(path: &Path, devno: DevNo) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let rdev = libc::makedev(devno.major, devno.minor);
    match std::fs::metadata(path) {
        Ok(meta) if meta.rdev() == rdev => return Ok(()),
        Ok(_) => std::fs::remove_file(path)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| invalid("device node path contains NUL"))?;
    // SAFETY: `c_path` is a valid NUL-terminated string for the call.
    let rc = unsafe { libc::mknod(c_path.as_ptr(), libc::S_IFBLK | 0o600, rdev) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

async fn run(program: &str, args: &[&str]) -> io::Result<String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Err(io::Error::other(format!(
        "{program} {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

/// Every thin id the pool holds. Needs a reserved metadata snapshot.
pub async fn thin_dump_ids(metadata: &Path) -> io::Result<Vec<u32>> {
    let text = run(
        "thin_dump",
        &["--skip-mappings", "-m", &metadata.to_string_lossy()],
    )
    .await?;
    parse::device_ids(&text)
}

/// Blocks that differ between two thins. Needs a reserved metadata
/// snapshot.
pub async fn thin_delta(metadata: &Path, left: u32, right: u32) -> io::Result<Vec<u64>> {
    let text = run(
        "thin_delta",
        &[
            "-m",
            "--snap1",
            &left.to_string(),
            "--snap2",
            &right.to_string(),
            &metadata.to_string_lossy(),
        ],
    )
    .await?;
    parse::delta_blocks(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_codes_match_the_kernel_header() {
        // musl types the request as `c_int`, so compare the bit pattern.
        assert_eq!(request(DM_DEV_CREATE_CMD) as u32, 0xc138fd03);
        assert_eq!(request(DM_TABLE_STATUS_CMD) as u32, 0xc138fd0c);
        assert_eq!(request(DM_TARGET_MSG_CMD) as u32, 0xc138fd0e);
    }

    #[test]
    fn a_target_spec_is_packed_padded_and_counted() {
        let mut req = Request::new("v1", 0).unwrap();
        req.push_target(4096, "thin", "252:0 1");
        let spec = &req.buf[HEADER_LEN..];
        assert_eq!(spec.len(), TARGET_SPEC_LEN + 8);
        assert_eq!(
            &spec[SPEC_OFF_LENGTH..SPEC_OFF_LENGTH + 8],
            &4096u64.to_ne_bytes()
        );
        assert_eq!(
            &spec[SPEC_OFF_NEXT..SPEC_OFF_NEXT + 4],
            &48u32.to_ne_bytes()
        );
        assert_eq!(&spec[SPEC_OFF_TYPE..SPEC_OFF_TYPE + 5], b"thin\0");
        assert_eq!(&spec[TARGET_SPEC_LEN..TARGET_SPEC_LEN + 8], b"252:0 1\0");
        assert_eq!(u32::from_ne_bytes(req.field(OFF_TARGET_COUNT)), 1);
        assert_eq!(req.targets().unwrap()[0].params, "252:0 1");
    }

    #[test]
    fn a_reply_with_two_targets_reads_both() {
        let mut req = Request::new("v1", 0).unwrap();
        req.push_target(10, "linear", "8:0 0");
        req.push_target(20, "thin", "252:0 7");
        assert_eq!(
            req.targets().unwrap(),
            vec![
                Target {
                    length_sectors: 10,
                    kind: "linear".into(),
                    params: "8:0 0".into()
                },
                Target {
                    length_sectors: 20,
                    kind: "thin".into(),
                    params: "252:0 7".into()
                },
            ]
        );
    }

    #[test]
    fn the_dev_field_decodes_the_kernel_huge_encoding() {
        let mut req = Request::new("p", 0).unwrap();
        let huge: u64 = (252 << 8) | 3 | ((0x100 & 0xfff00) << 12);
        req.buf[OFF_DEV..OFF_DEV + 8].copy_from_slice(&huge.to_ne_bytes());
        assert_eq!(
            req.dev(),
            DevNo {
                major: 252,
                minor: 0x103
            }
        );
    }

    #[test]
    fn a_name_too_long_for_the_header_is_refused() {
        assert!(Request::new(&"x".repeat(NAME_LEN), 0).is_err());
        assert!(Request::new(&"x".repeat(NAME_LEN - 1), 0).is_ok());
    }
}
