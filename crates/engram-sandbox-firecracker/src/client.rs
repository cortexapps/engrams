//! HTTP-over-Unix-socket client for the Firecracker control API.
//!
//! Each `FirecrackerBackend`-managed VM has its own Firecracker process
//! listening on a per-sandbox Unix socket; this client speaks the
//! swagger-defined API documented in `firecracker/swagger/firecracker.yaml`.
//!
//! The endpoints we cover:
//!
//! | Method  | Path                       | When                              |
//! |---------|----------------------------|-----------------------------------|
//! | `PUT`   | `/machine-config`          | configure on create               |
//! | `PUT`   | `/boot-source`             | configure on create               |
//! | `PUT`   | `/drives/{id}`             | attach rootfs / scratch disks     |
//! | `PUT`   | `/network-interfaces/{id}` | attach TAP (Phase 2 step 2)       |
//! | `PUT`   | `/vsock`                   | attach virtio-vsock (Phase 2 s2)  |
//! | `PUT`   | `/actions`                 | `InstanceStart`, `SendCtrlAltDel` |
//! | `PATCH` | `/vm`                      | `Paused` / `Resumed` (snapshots)  |
//! | `PUT`   | `/snapshot/create`         | take a Full or Diff snapshot      |
//! | `PUT`   | `/snapshot/load`           | resume from snapshot (with UFFD)  |
//!
//! # Wire format
//!
//! Manual HTTP/1.1 over `tokio::net::UnixStream`. Two non-obvious bits
//! that drove the implementation, both verified against upstream:
//!
//! 1. **Keep-alive is the default.** Firecracker's API server is built
//!    on `micro-http` and constructs every response with
//!    `Response::new(Version::Http11, ...)` — see
//!    `src/firecracker/src/api_server/mod.rs`. The server doesn't
//!    inspect `Connection: close`; the canonical
//!    [firecracker-go-sdk](https://github.com/firecracker-microvm/firecracker-go-sdk/blob/main/client_transports.go)
//!    relies on Go's default `http.Transport` which does HTTP/1.1
//!    keep-alive + connection pooling. We mirror those semantics:
//!    omit `Connection: close`, never half-close the write half.
//!
//! 2. **Content-Length framing both directions.** Because the server
//!    leaves the connection open, the client MUST read exactly
//!    `Content-Length` bytes — `read_to_end` would block until the
//!    server's keep-alive timeout. We open one connection per request
//!    for now (cheap on a Unix socket); a future optimisation is a
//!    pooled long-lived connection, matching go-sdk.
//!
//! A full `hyper` stack would handle this for us, but at the cost of
//! ~50 transitive dependencies for a dozen short PUTs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use engram_core::SandboxError;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Client bound to one Firecracker control socket. Cheap to clone —
/// the actual HTTP connection is established per-request.
#[derive(Clone, Debug)]
pub struct FirecrackerClient {
    socket: PathBuf,
    timeout: Duration,
}

/// How long any single API call is allowed to take. Firecracker's
/// control plane is supposed to respond synchronously and quickly; if
/// it doesn't, something is wrong (deadlock, dropped socket).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

impl FirecrackerClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Override the per-request timeout. Useful for snapshot/load which
    /// can stall on a slow network filesystem.
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    // ---- typed PUT/PATCH wrappers ----------------------------------

    /// `PUT /machine-config` — vCPU count, memory, SMT.
    pub async fn put_machine_config(&self, cfg: &MachineConfig) -> Result<(), SandboxError> {
        self.put("/machine-config", cfg).await
    }

    /// `PUT /boot-source` — kernel image + boot args (and optional initrd).
    pub async fn put_boot_source(&self, src: &BootSource) -> Result<(), SandboxError> {
        self.put("/boot-source", src).await
    }

    /// `PUT /drives/{drive_id}` — attach a block device. Set
    /// `is_root_device=true` for the rootfs.
    pub async fn put_drive(&self, drive: &DriveConfig) -> Result<(), SandboxError> {
        let path = format!("/drives/{}", drive.drive_id);
        self.put(&path, drive).await
    }

    /// `PUT /vsock` — attach a virtio-vsock device. The host-side `uds_path`
    /// is where `engram-agentd` will accept connections from inside the guest.
    pub async fn put_vsock(&self, vsock: &VsockConfig) -> Result<(), SandboxError> {
        self.put("/vsock", vsock).await
    }

    /// `PUT /actions` — `InstanceStart`, `SendCtrlAltDel`, etc.
    pub async fn put_action(&self, action: ActionType) -> Result<(), SandboxError> {
        let body = ActionBody {
            action_type: action,
        };
        self.put("/actions", &body).await
    }

    /// `PATCH /vm` — transition VM state to `Paused` or `Resumed`.
    pub async fn patch_vm_state(&self, state: VmState) -> Result<(), SandboxError> {
        let body = VmStatePatch { state };
        self.request_with_body("PATCH", "/vm", Some(&body)).await?;
        Ok(())
    }

    // ---- snapshot operations ----------------------------------------

    /// Atomically pause → snapshot → resume, writing `state.bin` and
    /// `memory.bin` to `dir`. Returns the resolved paths so the caller
    /// can hand them to a future `load_snapshot`.
    ///
    /// `dir` must already exist and be writable by the firecracker
    /// process (which on a non-jailer setup is just the host user).
    /// The VM stays running on success — snapshots are save-points,
    /// not eviction.
    ///
    /// On failure the VM may be left paused. Callers should follow up
    /// with `resume()` if the error originated *after* `pause` but
    /// before the implicit resume below.
    pub async fn create_snapshot(&self, dir: &Path) -> Result<SnapshotPaths, SandboxError> {
        let state_path = dir.join("state.bin");
        let mem_path = dir.join("memory.bin");

        self.pause().await?;

        let body = SnapshotCreateBody {
            snapshot_path: state_path.to_string_lossy().into_owned(),
            mem_file_path: mem_path.to_string_lossy().into_owned(),
            snapshot_type: SnapshotType::Full,
        };
        let create_res = self.put("/snapshot/create", &body).await;

        // Always try to resume, regardless of whether the create
        // succeeded — leaving the VM paused on error is worse than
        // doubling up on the failure path.
        let resume_res = self.resume().await;

        create_res?;
        resume_res?;
        Ok(SnapshotPaths {
            state_path,
            mem_path,
        })
    }

    /// Restore from `paths` with file-backed memory. Returns once the
    /// VM is fully running again (`resume_vm: true`).
    ///
    /// UFFD-backed restore (the load-bearing perf optimisation for
    /// fast resume) lands as a separate `load_snapshot_uffd` once the
    /// userfaultfd handler crate is in place — that needs `unsafe`
    /// for the syscall and deserves its own scrutiny.
    pub async fn load_snapshot(&self, paths: &SnapshotPaths) -> Result<(), SandboxError> {
        let body = SnapshotLoadBody {
            snapshot_path: paths.state_path.to_string_lossy().into_owned(),
            mem_backend: MemBackend {
                backend_type: MemBackendType::File,
                backend_path: paths.mem_path.to_string_lossy().into_owned(),
            },
            enable_diff_snapshots: false,
            resume_vm: true,
        };
        self.put("/snapshot/load", &body).await
    }

    pub async fn pause(&self) -> Result<(), SandboxError> {
        self.patch_vm_state(VmState::Paused).await
    }

    pub async fn resume(&self) -> Result<(), SandboxError> {
        self.patch_vm_state(VmState::Resumed).await
    }

    // ---- transport --------------------------------------------------

    /// One-shot PUT helper for endpoints that just acknowledge with 204.
    async fn put<B: Serialize>(&self, path: &str, body: &B) -> Result<(), SandboxError> {
        self.request_with_body("PUT", path, Some(body)).await?;
        Ok(())
    }

    /// Send one HTTP request over a fresh UnixStream. Returns the
    /// response body bytes on 2xx, or a structured error built from
    /// Firecracker's `{"fault_message": ...}` payload on 4xx/5xx.
    async fn request_with_body<B: Serialize>(
        &self,
        method: &str,
        path: &str,
        body: Option<&B>,
    ) -> Result<Vec<u8>, SandboxError> {
        let body_bytes = match body {
            Some(b) => serde_json::to_vec(b)
                .map_err(|e| vm_err(format!("serialize {method} {path}: {e}")))?,
            None => Vec::new(),
        };

        let fut = self.do_request(method, path, &body_bytes);
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(res) => res,
            Err(_) => Err(vm_err(format!(
                "Firecracker {method} {path} timed out after {:?}",
                self.timeout
            ))),
        }
    }

    async fn do_request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<Vec<u8>, SandboxError> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| vm_err(format!("connect {}: {e}", self.socket.display())))?;

        // Headers match what curl sends. We deliberately do NOT send
        // `Connection: close` or call `stream.shutdown()`: Firecracker's
        // micro-http reacts to either by RST'ing or hanging instead of
        // replying. Connection lifetime is governed by Content-Length
        // framing on both directions — we read exactly the response
        // body and then drop the stream.
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\n\
             Host: localhost\r\n\
             Accept: application/json\r\n\
             User-Agent: engram-sandbox-firecracker/0\r\n\
             Content-Length: {len}\r\n",
            len = body.len(),
        );
        if !body.is_empty() {
            req.push_str("Content-Type: application/json\r\n");
        }
        req.push_str("\r\n");

        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| vm_err(format!("write request: {e}")))?;
        if !body.is_empty() {
            stream
                .write_all(body)
                .await
                .map_err(|e| vm_err(format!("write body: {e}")))?;
        }

        // Read headers + body using Content-Length framing. We can't
        // read-to-EOF because micro-http keeps the connection open.
        let raw = read_http_response(&mut stream).await?;
        parse_response(method, path, &raw)
    }
}

/// Read one complete HTTP/1.1 response from `stream` using
/// Content-Length framing. We need framing rather than read-to-EOF
/// because Firecracker's micro-http leaves the connection open and
/// would let us hang forever otherwise.
async fn read_http_response(stream: &mut UnixStream) -> Result<Vec<u8>, SandboxError> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];

    // Read until we've seen the end of the headers (\r\n\r\n).
    let header_end = loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| vm_err(format!("read headers: {e}")))?;
        if n == 0 {
            return Err(vm_err("server closed before sending headers"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_double_crlf(&buf) {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err(vm_err("response headers exceed 64KB"));
        }
    };

    // Parse Content-Length out of the headers we have so far.
    let header_block = std::str::from_utf8(&buf[..header_end])
        .map_err(|e| vm_err(format!("non-UTF8 headers: {e}")))?;
    let content_length = header_block
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.eq_ignore_ascii_case("content-length") {
                v.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);

    // Read remaining body bytes if we don't already have them buffered.
    let body_start = header_end + 4;
    let body_have = buf.len().saturating_sub(body_start);
    if body_have < content_length {
        let mut remaining = content_length - body_have;
        while remaining > 0 {
            let n = stream
                .read(&mut chunk)
                .await
                .map_err(|e| vm_err(format!("read body: {e}")))?;
            if n == 0 {
                return Err(vm_err(format!(
                    "server closed mid-body ({remaining} bytes still expected)"
                )));
            }
            buf.extend_from_slice(&chunk[..n.min(remaining)]);
            remaining = remaining.saturating_sub(n);
        }
    }

    // We deliberately stop reading at Content-Length even if more bytes
    // are available — anything after this byte belongs to a future
    // response on the same (kept-alive) connection.
    Ok(buf[..body_start + content_length].to_vec())
}

/// Parse a complete HTTP/1.1 response. Splits headers from body on the
/// first `\r\n\r\n`, extracts the status code, and on non-2xx tries to
/// pull `fault_message` out of Firecracker's JSON error envelope.
fn parse_response(method: &str, path: &str, raw: &[u8]) -> Result<Vec<u8>, SandboxError> {
    let split_at = find_double_crlf(raw)
        .ok_or_else(|| vm_err(format!("{method} {path}: response missing CRLFCRLF")))?;

    let header_block = std::str::from_utf8(&raw[..split_at])
        .map_err(|e| vm_err(format!("{method} {path}: non-UTF8 headers: {e}")))?;
    let body = raw[split_at + 4..].to_vec();

    let status_line = header_block
        .split("\r\n")
        .next()
        .ok_or_else(|| vm_err(format!("{method} {path}: empty headers")))?;
    let code = parse_status_code(status_line).ok_or_else(|| {
        vm_err(format!(
            "{method} {path}: malformed status: {status_line:?}"
        ))
    })?;

    if (200..300).contains(&code) {
        return Ok(body);
    }

    // 4xx/5xx — try to surface Firecracker's structured fault_message.
    let detail = serde_json::from_slice::<FaultMessage>(&body)
        .map(|f| f.fault_message)
        .unwrap_or_else(|_| String::from_utf8_lossy(&body[..body.len().min(512)]).into_owned());
    Err(vm_err(format!(
        "Firecracker {method} {path} -> {code}: {detail}"
    )))
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_status_code(line: &str) -> Option<u16> {
    // "HTTP/1.1 204 No Content" -> 204
    line.split_whitespace().nth(1)?.parse().ok()
}

fn vm_err(msg: impl Into<String>) -> SandboxError {
    SandboxError::Vm(msg.into().into())
}

// ---- Wire types ------------------------------------------------------
//
// Field names match the Firecracker swagger schema; serde_json picks
// snake_case automatically so we don't need rename_all.

#[derive(Debug, Clone, Serialize)]
pub struct MachineConfig {
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
    /// Symmetric multi-threading: `false` for the safest default.
    pub smt: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BootSource {
    pub kernel_image_path: String,
    /// e.g. `"console=ttyS0 reboot=k panic=1 pci=off"`. Firecracker
    /// passes this verbatim to the kernel command line.
    pub boot_args: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initrd_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DriveConfig {
    pub drive_id: String,
    pub path_on_host: String,
    pub is_root_device: bool,
    pub is_read_only: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VsockConfig {
    pub guest_cid: u32,
    /// Host-side Unix socket where the agent connects. Firecracker
    /// proxies guest→host vsock connections to this UDS.
    pub uds_path: String,
}

/// `PATCH /vm` payload. PascalCase variant names match Firecracker's
/// expected JSON values (`{"state": "Paused"}`).
#[derive(Debug, Clone, Copy, Serialize)]
pub enum VmState {
    Paused,
    Resumed,
}

#[derive(Debug, Serialize)]
struct VmStatePatch {
    state: VmState,
}

/// `PUT /actions` payload. PascalCase matches Firecracker's expected
/// JSON values; serde uses Rust spellings by default for plain enums.
#[derive(Debug, Clone, Copy, Serialize)]
pub enum ActionType {
    InstanceStart,
    FlushMetrics,
    SendCtrlAltDel,
}

#[derive(Debug, Serialize)]
struct ActionBody {
    action_type: ActionType,
}

/// Pair of files Firecracker writes for a Full snapshot. `state_path`
/// is small (KBs); `mem_path` is the size of the guest's RAM.
#[derive(Clone, Debug)]
pub struct SnapshotPaths {
    pub state_path: PathBuf,
    pub mem_path: PathBuf,
}

/// `PUT /snapshot/create` payload. `snapshot_type` is `Full` for a
/// self-contained snapshot, `Diff` for an incremental one against the
/// previous Full. We only emit `Full` today.
#[derive(Debug, Serialize)]
struct SnapshotCreateBody {
    snapshot_path: String,
    mem_file_path: String,
    snapshot_type: SnapshotType,
}

/// PascalCase variant names match Firecracker's expected JSON values
/// (`{"snapshot_type": "Full"}`).
#[derive(Debug, Clone, Copy, Serialize)]
pub enum SnapshotType {
    Full,
    Diff,
}

/// `PUT /snapshot/load` payload.
#[derive(Debug, Serialize)]
struct SnapshotLoadBody {
    snapshot_path: String,
    mem_backend: MemBackend,
    enable_diff_snapshots: bool,
    /// `true` makes Firecracker resume the guest immediately after
    /// load (no separate `PATCH /vm Resumed` needed).
    resume_vm: bool,
}

/// How memory is supplied during snapshot load.
///   - `File`: Firecracker reads the memory file synchronously. Slow
///     start but simple.
///   - `Uffd`: a separate userfaultfd handler streams pages on demand.
///     Sub-100ms restore, but needs the handler process / `unsafe`.
///     We don't emit this variant today; it lands with the UFFD
///     handler slice.
#[derive(Debug, Serialize)]
struct MemBackend {
    backend_type: MemBackendType,
    backend_path: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
enum MemBackendType {
    File,
    #[allow(dead_code)] // surfaced when the UFFD slice lands
    Uffd,
}

#[derive(Debug, Deserialize)]
struct FaultMessage {
    fault_message: String,
}

// ---- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_config_serializes_to_swagger_field_names() {
        let cfg = MachineConfig {
            vcpu_count: 2,
            mem_size_mib: 1024,
            smt: false,
        };
        let v: serde_json::Value = serde_json::to_value(&cfg).unwrap();
        assert_eq!(v["vcpu_count"], 2);
        assert_eq!(v["mem_size_mib"], 1024);
        assert_eq!(v["smt"], false);
    }

    #[test]
    fn boot_source_omits_initrd_when_none() {
        // Firecracker rejects unknown fields; serializing `null` for
        // initrd_path would be different from omitting. Lock that down.
        let src = BootSource {
            kernel_image_path: "/k".into(),
            boot_args: "console=ttyS0".into(),
            initrd_path: None,
        };
        let s = serde_json::to_string(&src).unwrap();
        assert!(
            !s.contains("initrd_path"),
            "initrd_path should be omitted: {s}"
        );
    }

    #[test]
    fn drive_config_uses_swagger_field_names() {
        let d = DriveConfig {
            drive_id: "rootfs".into(),
            path_on_host: "/p".into(),
            is_root_device: true,
            is_read_only: false,
        };
        let v: serde_json::Value = serde_json::to_value(&d).unwrap();
        assert_eq!(v["drive_id"], "rootfs");
        assert_eq!(v["is_root_device"], true);
        assert_eq!(v["is_read_only"], false);
    }

    #[test]
    fn action_type_serializes_pascal_case() {
        // Firecracker expects exactly these strings.
        assert_eq!(
            serde_json::to_string(&ActionType::InstanceStart).unwrap(),
            "\"InstanceStart\""
        );
        assert_eq!(
            serde_json::to_string(&ActionType::SendCtrlAltDel).unwrap(),
            "\"SendCtrlAltDel\""
        );
    }

    #[test]
    fn vm_state_serializes_pascal_case() {
        assert_eq!(
            serde_json::to_string(&VmState::Paused).unwrap(),
            "\"Paused\""
        );
        assert_eq!(
            serde_json::to_string(&VmState::Resumed).unwrap(),
            "\"Resumed\""
        );
    }

    #[test]
    fn parse_response_204_returns_empty_body() {
        let raw = b"HTTP/1.1 204 No Content\r\nServer: Firecracker API\r\n\r\n";
        let body = parse_response("PUT", "/machine-config", raw).unwrap();
        assert!(body.is_empty());
    }

    #[test]
    fn parse_response_200_returns_body_bytes() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"state\":\"Running\"}";
        let body = parse_response("GET", "/", raw).unwrap();
        assert_eq!(body, b"{\"state\":\"Running\"}");
    }

    #[test]
    fn parse_response_400_surfaces_fault_message() {
        // Firecracker's wire format for control-plane errors. Locking
        // it in so a regression in error parsing fails loudly.
        let raw = b"HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\r\n{\"fault_message\":\"vcpu_count must be > 0\"}";
        let err = parse_response("PUT", "/machine-config", raw).unwrap_err();
        let SandboxError::Vm(b) = err else {
            panic!("expected Vm error");
        };
        let msg = b.to_string();
        assert!(msg.contains("400"), "{msg}");
        assert!(msg.contains("vcpu_count must be > 0"), "{msg}");
        assert!(msg.contains("/machine-config"), "{msg}");
    }

    #[test]
    fn parse_response_falls_back_to_raw_body_when_not_json() {
        let raw = b"HTTP/1.1 500 Internal Server Error\r\n\r\nplain text oops";
        let err = parse_response("PUT", "/x", raw).unwrap_err();
        assert!(err.to_string().contains("plain text oops"));
    }

    #[test]
    fn parse_response_rejects_malformed_input() {
        // No CRLFCRLF, no status line — must error rather than panic.
        let raw = b"this is not http";
        assert!(parse_response("PUT", "/x", raw).is_err());
    }

    #[test]
    fn find_double_crlf_finds_boundary() {
        assert_eq!(find_double_crlf(b"abc\r\n\r\nbody"), Some(3));
        assert_eq!(find_double_crlf(b"\r\n\r\n"), Some(0));
        assert_eq!(find_double_crlf(b"no boundary"), None);
    }

    #[test]
    fn parse_status_code_handles_well_formed_status_line() {
        assert_eq!(parse_status_code("HTTP/1.1 204 No Content"), Some(204));
        assert_eq!(parse_status_code("HTTP/1.1 400 Bad Request"), Some(400));
    }

    #[test]
    fn snapshot_create_body_uses_swagger_field_names() {
        let body = SnapshotCreateBody {
            snapshot_path: "/snap/state.bin".into(),
            mem_file_path: "/snap/memory.bin".into(),
            snapshot_type: SnapshotType::Full,
        };
        let v: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(v["snapshot_path"], "/snap/state.bin");
        assert_eq!(v["mem_file_path"], "/snap/memory.bin");
        assert_eq!(v["snapshot_type"], "Full");
    }

    #[test]
    fn snapshot_load_body_uses_file_backend_by_default() {
        // PUT /snapshot/load with backend_type=File is what the
        // FirecrackerClient::load_snapshot helper builds. Lock the
        // wire shape so a serde rename refactor would fail loud.
        let body = SnapshotLoadBody {
            snapshot_path: "/snap/state.bin".into(),
            mem_backend: MemBackend {
                backend_type: MemBackendType::File,
                backend_path: "/snap/memory.bin".into(),
            },
            enable_diff_snapshots: false,
            resume_vm: true,
        };
        let v: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(v["snapshot_path"], "/snap/state.bin");
        assert_eq!(v["mem_backend"]["backend_type"], "File");
        assert_eq!(v["mem_backend"]["backend_path"], "/snap/memory.bin");
        assert_eq!(v["enable_diff_snapshots"], false);
        assert_eq!(v["resume_vm"], true);
    }

    #[test]
    fn snapshot_type_serializes_pascal_case() {
        assert_eq!(
            serde_json::to_string(&SnapshotType::Full).unwrap(),
            "\"Full\""
        );
        assert_eq!(
            serde_json::to_string(&SnapshotType::Diff).unwrap(),
            "\"Diff\""
        );
    }
}
