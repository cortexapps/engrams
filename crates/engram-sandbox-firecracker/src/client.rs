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
//! | `PUT`   | `/network-interfaces/{id}` | attach TAP                        |
//! | `PUT`   | `/vsock`                   | attach virtio-vsock               |
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

    /// `PATCH /drives/{drive_id}` — swap the host file backing an
    /// already-attached drive. Valid on a paused VM (including a
    /// VM that was loaded from a snapshot but not yet resumed),
    /// which is the ADR 0014 option-D restore path: load snapshot
    /// paused → patch harness drive → resume → guest sees the new
    /// file's bytes.
    ///
    /// On resume, FC re-notifies the virtio-blk queues for every
    /// drive ("Artificially kick devices" in its logs); the guest
    /// kernel responds by re-reading capacity and, on size change,
    /// invalidates its buffer cache for the device. So even a
    /// guest that read from the drive pre-snapshot will see the
    /// new bytes on the next read post-resume — confirmed by
    /// `tests/patch_drive_swap.rs`.
    pub async fn patch_drive(
        &self,
        drive_id: &str,
        path_on_host: &Path,
    ) -> Result<(), SandboxError> {
        let body = DrivePatchBody {
            drive_id: drive_id.to_string(),
            path_on_host: path_on_host.to_string_lossy().into_owned(),
        };
        let path = format!("/drives/{drive_id}");
        self.request_with_body("PATCH", &path, Some(&body)).await?;
        Ok(())
    }

    /// `PUT /vsock` — attach a virtio-vsock device. The host-side `uds_path`
    /// is where `engram-agentd` will accept connections from inside the guest.
    pub async fn put_vsock(&self, vsock: &VsockConfig) -> Result<(), SandboxError> {
        self.put("/vsock", vsock).await
    }

    /// `PUT /network-interfaces/{iface_id}` — attach a virtio-net
    /// device backed by a host TAP. The TAP must already exist
    /// (created via `ip tuntap add` before this call). Firecracker
    /// surfaces the device to the guest as `eth0` for the first
    /// configured interface.
    pub async fn put_network_interface(
        &self,
        iface: &NetworkInterface,
    ) -> Result<(), SandboxError> {
        let path = format!("/network-interfaces/{}", iface.iface_id);
        self.put(&path, iface).await
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
        self.create_snapshot_at(
            dir.join("state.bin"),
            dir.join("memory.bin"),
            SnapshotType::Full,
        )
        .await
    }

    /// `create_snapshot` with explicit output paths and snapshot type.
    /// ADR 0028: `SnapshotType::Diff` writes only pages dirtied since
    /// the last capture (sparse file) and resets the KVM dirty bitmap —
    /// requires dirty tracking (`MachineConfig.track_dirty_pages` on a
    /// cold boot, or `enable_diff_snapshots` at `snapshot/load`).
    /// Same pause/resume + cancellation-guard contract as
    /// [`Self::create_snapshot`].
    pub async fn create_snapshot_at(
        &self,
        state_path: PathBuf,
        mem_path: PathBuf,
        snapshot_type: SnapshotType,
    ) -> Result<SnapshotPaths, SandboxError> {
        self.pause().await?;

        // ADR 0016 §A.1.2: tokio cancellation between `pause` and the
        // final `resume` below leaves the VM Paused, because async
        // Drop can't run async cleanup. A `ResumeOnDrop` guard catches
        // that case — if our future gets cancelled (caller times out,
        // task is aborted, panic unwinds), the guard's Drop spawns a
        // detached resume so the VM eventually unpauses. On the happy
        // path we explicitly disarm before the inline resume so we
        // don't double-resume.
        let mut guard = ResumeOnDrop::arm(self.clone());

        let body = SnapshotCreateBody {
            snapshot_path: state_path.to_string_lossy().into_owned(),
            mem_file_path: mem_path.to_string_lossy().into_owned(),
            snapshot_type,
            vmstate_only: false,
        };
        let create_res = self.put("/snapshot/create", &body).await;

        // Always try to resume, regardless of whether the create
        // succeeded — leaving the VM paused on error is worse than
        // doubling up on the failure path.
        let resume_res = self.resume().await;
        guard.disarm();

        create_res?;
        resume_res?;
        Ok(SnapshotPaths {
            state_path,
            mem_path,
        })
    }

    /// ADR 0045 C2 (fork v3): vmstate-only snapshot create — writes the
    /// state file at `state_path` and skips the guest-memory leg entirely.
    /// The post-copy blackout primitive: the destination demand-faults the
    /// dirty memory from this (still paused) source's address space, so no
    /// memory file ever materializes.
    ///
    /// Deliberately NO pause/resume wrapper and NO `ResumeOnDrop` guard:
    /// the migration blackout owns the pause, and an auto-resume of the
    /// source mid-move is exactly the split-brain this rung is built to
    /// prevent. The caller must hold the VM Paused before calling and owns
    /// whatever happens to it afterwards (commit-destroy or abort-resume).
    ///
    /// Requires the fork-v3 binary: stock/older-fork FC rejects the
    /// `vmstate_only` field (`deny_unknown_fields`), surfacing as a PUT
    /// error — the capability gate for mixed-fleet rolls.
    pub async fn create_snapshot_vmstate_only(
        &self,
        state_path: &Path,
    ) -> Result<(), SandboxError> {
        let body = SnapshotCreateBody {
            snapshot_path: state_path.to_string_lossy().into_owned(),
            // Ignored by fork v3; a sibling placeholder keeps the field
            // present (it is required by FC's schema).
            mem_file_path: state_path
                .with_extension("memfile-ignored")
                .to_string_lossy()
                .into_owned(),
            snapshot_type: SnapshotType::Full,
            vmstate_only: true,
        };
        self.put("/snapshot/create", &body).await
    }

    /// Restore from `paths` with file-backed memory. Returns once the
    /// VM is fully running again (`resume_vm: true`).
    ///
    /// File mode synchronously reads `mem_path` into the guest's
    /// address space — fine for dev/test loops but slow for fast
    /// eviction-resume. Use [`Self::load_snapshot_uffd`] in
    /// production-grade resume paths.
    pub async fn load_snapshot(&self, paths: &SnapshotPaths) -> Result<(), SandboxError> {
        self.load_snapshot_inner(paths, /*resume_vm=*/ true, /*track=*/ false, None)
            .await
    }

    /// Load a snapshot into a paused VM. Caller is responsible for
    /// the eventual `patch_vm_state(Resumed)`. Used by the ADR 0014
    /// option-D restore path where a `patch_drive` happens between
    /// load and resume.
    pub async fn load_snapshot_paused(&self, paths: &SnapshotPaths) -> Result<(), SandboxError> {
        self.load_snapshot_inner(
            paths, /*resume_vm=*/ false, /*track=*/ false, None,
        )
        .await
    }

    /// File-backed load with explicit `resume_vm` /
    /// `enable_diff_snapshots` knobs. ADR 0028: restored VMs have no
    /// `machine-config` PUT, so `enable_diff_snapshots: true` here is
    /// the only way to (re-)arm KVM dirty tracking for subsequent
    /// `SnapshotType::Diff` captures. `vsock_uds_override` re-keys the
    /// vsock UDS to a per-sandbox path (upstream FC ≥1.16; see
    /// [`SnapshotLoadBody::vsock_override`]).
    pub async fn load_snapshot_opts(
        &self,
        paths: &SnapshotPaths,
        resume_vm: bool,
        enable_diff_snapshots: bool,
        vsock_uds_override: Option<&Path>,
    ) -> Result<(), SandboxError> {
        self.load_snapshot_inner(paths, resume_vm, enable_diff_snapshots, vsock_uds_override)
            .await
    }

    async fn load_snapshot_inner(
        &self,
        paths: &SnapshotPaths,
        resume_vm: bool,
        enable_diff_snapshots: bool,
        vsock_uds_override: Option<&Path>,
    ) -> Result<(), SandboxError> {
        let body = SnapshotLoadBody {
            snapshot_path: paths.state_path.to_string_lossy().into_owned(),
            mem_backend: MemBackend {
                backend_type: MemBackendType::File,
                backend_path: paths.mem_path.to_string_lossy().into_owned(),
            },
            enable_diff_snapshots,
            resume_vm,
            // Substrate base backing is a Uffd-mode concept (ADR 0045 v2b).
            uffd_base_file: None,
            vsock_override: vsock_uds_override.map(|p| VsockOverrideBody {
                uds_path: p.to_string_lossy().into_owned(),
            }),
        };
        self.put("/snapshot/load", &body).await
    }

    /// Restore with UFFD-backed memory. `state_path` is the snapshot
    /// state file; `uffd_uds_path` points at the *already-listening*
    /// `engram-uffd-handler` UDS — Firecracker connects to it during
    /// this call, hands the kernel-side UFFD over SCM_RIGHTS, and
    /// resumes the guest. Pages stream in lazily on guest fault.
    ///
    /// The caller is responsible for spawning the handler (and
    /// keeping it alive for the VM's lifetime) before invoking this.
    pub async fn load_snapshot_uffd(
        &self,
        state_path: &Path,
        uffd_uds_path: &Path,
    ) -> Result<(), SandboxError> {
        self.load_snapshot_uffd_inner(
            state_path,
            uffd_uds_path,
            /*resume_vm=*/ true,
            false,
            None,
            None,
        )
        .await
    }

    /// UFFD-backed load that leaves the VM paused. Caller must
    /// follow up with `patch_vm_state(Resumed)`. Used by the ADR
    /// 0014 option-D restore path so the host can `patch_drive` on
    /// the harness substrate between load and resume — kernel
    /// hasn't started executing yet, so the patch redirects the
    /// next read of /dev/vdb to the session's harness ext4 instead
    /// of the bake-time stub.
    pub async fn load_snapshot_uffd_paused(
        &self,
        state_path: &Path,
        uffd_uds_path: &Path,
    ) -> Result<(), SandboxError> {
        self.load_snapshot_uffd_inner(
            state_path,
            uffd_uds_path,
            /*resume_vm=*/ false,
            false,
            None,
            None,
        )
        .await
    }

    /// UFFD-backed load with explicit `resume_vm` /
    /// `enable_diff_snapshots` knobs (ADR 0028; see
    /// [`Self::load_snapshot_opts`]). `vsock_uds_override` re-keys the
    /// vsock UDS to a per-sandbox path (upstream FC ≥1.16; see
    /// [`SnapshotLoadBody::vsock_override`]).
    pub async fn load_snapshot_uffd_opts(
        &self,
        state_path: &Path,
        uffd_uds_path: &Path,
        resume_vm: bool,
        enable_diff_snapshots: bool,
        uffd_base_file: Option<&Path>,
        vsock_uds_override: Option<&Path>,
    ) -> Result<(), SandboxError> {
        self.load_snapshot_uffd_inner(
            state_path,
            uffd_uds_path,
            resume_vm,
            enable_diff_snapshots,
            uffd_base_file,
            vsock_uds_override,
        )
        .await
    }

    async fn load_snapshot_uffd_inner(
        &self,
        state_path: &Path,
        uffd_uds_path: &Path,
        resume_vm: bool,
        enable_diff_snapshots: bool,
        uffd_base_file: Option<&Path>,
        vsock_uds_override: Option<&Path>,
    ) -> Result<(), SandboxError> {
        let body = SnapshotLoadBody {
            snapshot_path: state_path.to_string_lossy().into_owned(),
            mem_backend: MemBackend {
                backend_type: MemBackendType::Uffd,
                backend_path: uffd_uds_path.to_string_lossy().into_owned(),
            },
            enable_diff_snapshots,
            resume_vm,
            uffd_base_file: uffd_base_file.map(|p| p.to_string_lossy().into_owned()),
            vsock_override: vsock_uds_override.map(|p| VsockOverrideBody {
                uds_path: p.to_string_lossy().into_owned(),
            }),
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
    /// KVM dirty-page tracking (ADR 0028). Required for
    /// `SnapshotType::Diff` captures on a cold-created VM (restored
    /// VMs arm it via `enable_diff_snapshots` at `snapshot/load`).
    /// Always serialized — `false` matches FC's default, so the
    /// explicit field is behavior-identical for existing callers.
    pub track_dirty_pages: bool,
    /// CPUID mask the guest sees. `None` = host passthrough (FC's
    /// default) — guest CPUID echoes the underlying physical CPU,
    /// vendor and all. Snapshots taken under passthrough capture the
    /// bake-host CPUID into the vCPU state and break on restore when
    /// the receive host is a different CPU (observed prod 2026-05-21:
    /// AMD-baked snapshot restored on Intel Cascade Lake — guest's
    /// glibc ifunc resolver picked AMD-only AVX-512 paths and every
    /// `fork+exec+wait` of an external binary segfaulted the shell on
    /// exit cleanup at `RIP=0`). `Some("T2CL")` masks to a Cascade
    /// Lake baseline that's portable across any Intel CL-or-newer
    /// host AND, courtesy of FC's mask, across AMD bake runners too.
    /// Other valid values: `"T2"`, `"T2S"`, `"T2A"`, `"C3"`, plus
    /// FC's custom-template JSON form (see Firecracker's CPU
    /// templates doc).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_template: Option<String>,
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

/// `PATCH /drives/{drive_id}` body. Subset of `DriveConfig`:
/// `drive_id` + `path_on_host` are the only fields FC accepts on
/// patch (boot-time flags like `is_root_device` are immutable
/// post-attach).
#[derive(Debug, Clone, Serialize)]
struct DrivePatchBody {
    drive_id: String,
    path_on_host: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VsockConfig {
    pub guest_cid: u32,
    /// Host-side Unix socket where the agent connects. Firecracker
    /// proxies guest→host vsock connections to this UDS.
    pub uds_path: String,
}

/// `PUT /network-interfaces/{iface_id}` payload. The TAP at
/// `host_dev_name` must exist on the host already; Firecracker
/// opens it (CAP_NET_ADMIN required) and binds the virtio-net
/// frontend to it. `guest_mac` is optional — Firecracker generates
/// one if omitted, but pinning it makes guest-side udev rules and
/// debugging easier.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct NetworkInterface {
    pub iface_id: String,
    pub host_dev_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_mac: Option<String>,
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
    /// ADR 0045 C2 (fork v3): write only the vmstate file; the memory leg
    /// is skipped entirely and `mem_file_path` is ignored. Skipped from the
    /// wire when false so the body stays byte-identical to stock — stock FC
    /// rejects the unknown field (`deny_unknown_fields`), which is exactly
    /// the fork-v3 capability gate.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    vmstate_only: bool,
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
    /// ADR 0045 substrate (v2b): when set with the Uffd backend, the
    /// forked FC creates guest memory as `MAP_PRIVATE` of this shmem
    /// base file and registers UFFD `MISSING|MINOR`, letting the
    /// handler share base-identical pages across same-template VMs via
    /// `UFFDIO_CONTINUE`. `skip_serializing_if` keeps the body
    /// byte-identical to stock when unset (stock FC and pre-v2 forks
    /// `deny_unknown_fields` this struct).
    #[serde(skip_serializing_if = "Option::is_none")]
    uffd_base_file: Option<String>,
    /// Re-key the vsock backend UDS to a per-sandbox path at load,
    /// instead of the ancestor path embedded in `state.bin`. Without
    /// this, every VM descended from one base capture binds the SAME
    /// absolute UDS path — two same-image VMs on one host then fight
    /// over it and exec/harness traffic silently follows the last
    /// binder (the 2026-06-11 cross-session misroute). Upstream FC
    /// ≥1.16 (so every engrams build since the Phase B roll) rewrites
    /// the device state pre-build (`persist.rs` vsock_override), so FC
    /// binds this path and derives the `_<port>` dial-out paths from
    /// it too.
    #[serde(skip_serializing_if = "Option::is_none")]
    vsock_override: Option<VsockOverrideBody>,
}

/// `vsock_override` member of [`SnapshotLoadBody`].
#[derive(Debug, Serialize)]
struct VsockOverrideBody {
    uds_path: String,
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
    Uffd,
}

#[derive(Debug, Deserialize)]
struct FaultMessage {
    fault_message: String,
}

// ---- ADR 0016 §A.1.2: ResumeOnDrop guard ----------------------------
//
// FC operations that pause the VM (`create_snapshot`,
// `swap_harness_drive`) need a guarantee that the VM is resumed even
// if the future driving the operation is dropped mid-sequence —
// tokio task aborts, `tokio::time::timeout`, panic unwinds, caller
// dropped its handle. The inline `resume().await` after the paused
// work doesn't survive cancellation because the future is dropped
// before the await point is reached.
//
// Async Drop is the missing language feature here. The next-best
// pattern is a sync `Drop` that spawns a detached resume task. The
// VM will be unpaused on the next runtime tick, regardless of what
// happened to the original future. On the happy path the guard's
// `disarm` is called after the inline resume so we don't
// double-resume.
//
// Surfaced on 2026-05-23 during the COW diagnostic spot-check on
// session 96392fd3: 3 successful flushes on the host with 0 PG
// snapshot rows, then a follow-up prompt that the harness never
// processed — consistent with a partially-completed snapshot
// pipeline leaving the VM stuck Paused.

/// Defuse-on-success guard: if dropped while armed, spawns a
/// detached `resume` so the VM doesn't stay paused on cancellation.
/// Defuse via [`Self::disarm`] after the inline resume completes on
/// the happy path.
pub(crate) struct ResumeOnDrop {
    client: FirecrackerClient,
    armed: bool,
}

impl ResumeOnDrop {
    pub(crate) fn arm(client: FirecrackerClient) -> Self {
        Self {
            client,
            armed: true,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ResumeOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Spawn a detached task — Drop is sync; we can't await here.
        // The client is `Clone`; cloning is cheap (just a PathBuf +
        // Duration). The task runs on whatever runtime owned the
        // original future, which is still alive even if our future
        // was cancelled (tokio cancellation drops futures, not the
        // runtime).
        let client = self.client.clone();
        tokio::spawn(async move {
            if let Err(e) = client.resume().await {
                tracing::warn!(
                    socket = %client.socket().display(),
                    error = %e,
                    "ResumeOnDrop: detached resume after cancellation failed; \
                     VM may remain paused. ADR 0016 §A.1.2.",
                );
            } else {
                tracing::info!(
                    socket = %client.socket().display(),
                    "ResumeOnDrop: detached resume completed after cancellation",
                );
            }
        });
    }
}

// ---- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- ADR 0016 §A.1.2: ResumeOnDrop -------------------------------

    /// Spin up a minimal Unix-socket "FC" server that records every
    /// HTTP request it sees. Used by the ResumeOnDrop tests to assert
    /// the detached task actually issues `PATCH /vm` with
    /// `state=Resumed` on cancellation.
    ///
    /// Returns `(socket_path, handle)`. The handle exposes a channel
    /// of received `(method, path, body)` tuples and a tempdir guard
    /// keeping the socket alive for the duration of the test.
    async fn spawn_recording_server() -> (
        PathBuf,
        tempfile::TempDir,
        tokio::sync::mpsc::UnboundedReceiver<(String, String, String)>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    // Read until we've seen \r\n\r\n + content-length
                    // bytes. The clients in this crate write small
                    // requests (PATCH /vm payload is ~20 bytes), so a
                    // single read is plenty in practice.
                    let n = match stream.read(&mut buf).await {
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                    // Parse: METHOD PATH ... \r\n ... \r\n\r\nBODY
                    let mut lines = raw.split("\r\n");
                    let first = lines.next().unwrap_or("");
                    let mut parts = first.split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("").to_string();
                    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                    let _ = tx.send((method, path, body));
                    // Respond 204 No Content — FC's success shape.
                    let _ = stream
                        .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                        .await;
                });
            }
        });
        (socket, dir, rx)
    }

    #[tokio::test]
    async fn resume_on_drop_fires_when_armed() {
        // Construct a guard, drop it without disarming; expect the
        // detached task to PATCH /vm with state=Resumed.
        let (socket, _dir, mut rx) = spawn_recording_server().await;
        let client = FirecrackerClient::new(&socket);
        {
            let _guard = ResumeOnDrop::arm(client);
            // Guard dropped at end of scope (no disarm).
        }
        // Detached task runs on the runtime; give it a moment.
        let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("detached resume task should have run before timeout")
            .expect("channel sender survives");
        assert_eq!(received.0, "PATCH");
        assert_eq!(received.1, "/vm");
        assert!(
            received.2.contains("Resumed"),
            "body should request VmState::Resumed, got {}",
            received.2,
        );
    }

    #[tokio::test]
    async fn resume_on_drop_is_a_noop_when_disarmed() {
        // Happy path: caller resumed inline and disarmed the guard.
        // Drop must not spawn a second resume.
        let (socket, _dir, mut rx) = spawn_recording_server().await;
        let client = FirecrackerClient::new(&socket);
        {
            let mut guard = ResumeOnDrop::arm(client);
            guard.disarm();
        }
        // No spawn should happen. Wait briefly to confirm.
        let result = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        assert!(
            result.is_err(),
            "no resume should have fired after disarm; got {:?}",
            result.ok().flatten(),
        );
    }

    #[tokio::test]
    async fn cancellation_between_pause_and_resume_still_resumes_vm() {
        // The actual scenario A.1.2 fixes: a task is cancelled while
        // awaiting something inside the paused window. The guard's
        // Drop must fire a detached resume so the VM doesn't stay
        // paused. We simulate this by holding a ResumeOnDrop and
        // letting the future be dropped via tokio::time::timeout.
        let (socket, _dir, mut rx) = spawn_recording_server().await;
        let client = FirecrackerClient::new(&socket);

        // The future that times out holds the guard. On timeout the
        // future is dropped; the guard's Drop fires.
        let fut = async {
            let _guard = ResumeOnDrop::arm(client);
            // "Pause-then-PUT-then-resume" — simulate the long PUT
            // by sleeping past the timeout. The outer
            // `tokio::time::timeout` will cancel us mid-await.
            tokio::time::sleep(Duration::from_secs(60)).await;
            // We never reach here in this test — the timeout fires
            // first. If we DID reach here, we'd disarm before the
            // inline resume.
            unreachable!("test should have timed out by now");
        };
        let outcome = tokio::time::timeout(Duration::from_millis(50), fut).await;
        assert!(
            outcome.is_err(),
            "outer timeout must fire to exercise the cancellation path"
        );
        // After cancellation, the spawned resume should arrive at the
        // recording server.
        let received = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("detached resume must run after cancellation")
            .expect("channel sender survives");
        assert_eq!(received.0, "PATCH");
        assert_eq!(received.1, "/vm");
        assert!(received.2.contains("Resumed"));
    }

    #[test]
    fn machine_config_serializes_to_swagger_field_names() {
        let cfg = MachineConfig {
            vcpu_count: 2,
            mem_size_mib: 1024,
            smt: false,
            track_dirty_pages: false,
            cpu_template: None,
        };
        let v: serde_json::Value = serde_json::to_value(&cfg).unwrap();
        assert_eq!(v["vcpu_count"], 2);
        assert_eq!(v["mem_size_mib"], 1024);
        assert_eq!(v["smt"], false);
        // ADR 0028: always on the wire; `false` == FC default.
        assert_eq!(v["track_dirty_pages"], false);
        // Absent cpu_template must NOT appear on the wire (FC treats
        // `null` differently from omitted in some endpoints; we want
        // the host-passthrough default unchanged when the field is
        // not set).
        assert!(v.get("cpu_template").is_none());
    }

    #[test]
    fn machine_config_serializes_cpu_template_when_set() {
        let cfg = MachineConfig {
            vcpu_count: 2,
            mem_size_mib: 1024,
            smt: false,
            track_dirty_pages: true,
            cpu_template: Some("T2CL".into()),
        };
        let v: serde_json::Value = serde_json::to_value(&cfg).unwrap();
        assert_eq!(v["cpu_template"], "T2CL");
        assert_eq!(v["track_dirty_pages"], true);
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
            vmstate_only: false,
        };
        let v: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(v["snapshot_path"], "/snap/state.bin");
        assert_eq!(v["mem_file_path"], "/snap/memory.bin");
        assert_eq!(v["snapshot_type"], "Full");
        // ADR 0045 C2: when false the field is OMITTED — the body stays
        // byte-identical to stock so non-migration captures keep working
        // against any FC binary (R2 posture).
        assert!(v.get("vmstate_only").is_none());

        let body = SnapshotCreateBody {
            snapshot_path: "/snap/state.bin".into(),
            mem_file_path: "/snap/ignored".into(),
            snapshot_type: SnapshotType::Full,
            vmstate_only: true,
        };
        let v: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(v["vmstate_only"], true);
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
            uffd_base_file: None,
            vsock_override: None,
        };
        let v: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(v["snapshot_path"], "/snap/state.bin");
        assert_eq!(v["mem_backend"]["backend_type"], "File");
        assert_eq!(v["mem_backend"]["backend_path"], "/snap/memory.bin");
        assert_eq!(v["enable_diff_snapshots"], false);
        assert_eq!(v["resume_vm"], true);
        // Unset override is OMITTED — body stays byte-identical to
        // stock (deny_unknown_fields posture, same as uffd_base_file).
        assert!(v.get("vsock_override").is_none());

        let body = SnapshotLoadBody {
            snapshot_path: "/snap/state.bin".into(),
            mem_backend: MemBackend {
                backend_type: MemBackendType::Uffd,
                backend_path: "/snap/uffd.sock".into(),
            },
            enable_diff_snapshots: false,
            resume_vm: true,
            uffd_base_file: None,
            vsock_override: Some(VsockOverrideBody {
                uds_path: "/work/abc.vsock".into(),
            }),
        };
        let v: serde_json::Value = serde_json::to_value(&body).unwrap();
        // Wire shape matches the fork's swagger: a nested object with
        // `uds_path` (vsock re-key at load).
        assert_eq!(v["vsock_override"]["uds_path"], "/work/abc.vsock");
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
