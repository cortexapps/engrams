//! Per-sandbox networking for the Firecracker backend.
//!
//! Each VM gets a dedicated `/30` carved out of the host-agent's
//! configured engram CIDR (default `10.200.0.0/16`). The host owns
//! the gateway IP; the guest gets the next address. A TAP device
//! lives on the host, terminated at the gateway IP. Static IP via
//! the kernel `ip=` cmdline (`CONFIG_IP_PNP_*=y`) — no DHCP server
//! on the host, less attack surface.
//!
//! Egress is filtered through a per-VM iptables chain
//! `engram-sb-<id>` so each sandbox's policy is a scoped, yankable
//! object. The chain layers:
//!
//! 1. **Hard isolation** (always on): DROP traffic into RFC1918,
//!    link-local, loopback. The global FORWARD rule `-s 10.200/16
//!    -d 10.200/16 DROP` covers inter-VM blocking once.
//! 2. **DNS allow** for `1.1.1.1:53` UDP+TCP — the only
//!    unconditional egress. Everything else has to be in
//!    `allow_hosts`.
//! 3. **Resolved `manifest.network.allow_hosts`** — at create time
//!    we resolve each entry to IPv4s and ACCEPT them. Refresh task
//!    (separate module) re-resolves periodically to catch CDN
//!    rotation.
//! 4. **Final default** — DROP if `default = Deny` and
//!    `policy = Enforce`; LOG-and-ACCEPT under `LogOnly` so the PR
//!    can ship without breaking sessions whose manifests haven't
//!    declared their outbound destinations.
//!
//! Pure-Rust types here (`NetworkAllocator`, `IptablesRules`,
//! `tap_name_for`) are unit-tested on macOS; the runtime that
//! actually creates TAPs and shells out to `iptables` lives in
//! `lib.rs` under `#[cfg(target_os = "linux")]`.

use std::collections::HashSet;
use std::net::Ipv4Addr;

use engram_core::types::ids::SandboxId;
use engram_core::types::{NetworkDefault, NetworkPolicy};

/// One `/30` carved from the engram pool. Per RFC 3021 a /30 has 4
/// addresses: network, gateway, guest, broadcast. We assign:
///   - `.0` — network (unused)
///   - `.1` — host-side TAP gateway
///   - `.2` — guest's eth0
///   - `.3` — broadcast (unused)
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct VmCidr {
    /// Network address (`.0`).
    network: Ipv4Addr,
}

impl VmCidr {
    pub fn new(network: Ipv4Addr) -> Self {
        Self { network }
    }

    pub fn host(&self) -> Ipv4Addr {
        let o = self.network.octets();
        Ipv4Addr::new(o[0], o[1], o[2], o[3] | 0b01)
    }

    pub fn guest(&self) -> Ipv4Addr {
        let o = self.network.octets();
        Ipv4Addr::new(o[0], o[1], o[2], o[3] | 0b10)
    }

    /// `<network>/30` form for matching in iptables / netlink.
    pub fn cidr_str(&self) -> String {
        format!("{}/30", self.network)
    }

    /// Kernel `ip=` cmdline tail for the guest. Static config means
    /// no DHCP needed; CONFIG_IP_PNP brings up eth0 before init.
    pub fn kernel_ip_arg(&self) -> String {
        format!(
            "ip={}::{}:255.255.255.252::eth0:off",
            self.guest(),
            self.host()
        )
    }
}

/// Carves the host-agent's engram CIDR into /30s. Sequential
/// allocator with destroy-time recycle; in-memory only because
/// sandboxes don't outlive the host-agent.
#[derive(Debug)]
pub struct NetworkAllocator {
    pool_base: u32,
    pool_size: u32,
    next: u32,
    in_use: HashSet<u32>,
    free: Vec<u32>,
}

#[derive(Debug)]
pub enum AllocError {
    /// The pool's been fully carved up. With a /16 that's 16k
    /// concurrent sandboxes — way past anything the host can host.
    PoolExhausted,
}

impl std::fmt::Display for AllocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PoolExhausted => write!(f, "engram CIDR pool exhausted"),
        }
    }
}

impl std::error::Error for AllocError {}

impl NetworkAllocator {
    /// Build an allocator over `pool/16`. Lower octets must be 0
    /// (we carve exactly one /16 — `10.200.0.0/16` is the canonical
    /// default).
    pub fn new(pool: Ipv4Addr) -> Self {
        let octets = pool.octets();
        let base = u32::from_be_bytes([octets[0], octets[1], 0, 0]);
        Self {
            pool_base: base,
            // 16384 /30s in a /16.
            pool_size: 1 << 14,
            next: 0,
            in_use: HashSet::new(),
            free: Vec::new(),
        }
    }

    pub fn alloc(&mut self) -> Result<VmCidr, AllocError> {
        // Recycled slots first — keeps the address space dense
        // (lower /30s come back into rotation before we burn fresh
        // ones at the high end).
        let slot = if let Some(s) = self.free.pop() {
            s
        } else if self.next < self.pool_size {
            let s = self.next;
            self.next += 1;
            s
        } else {
            return Err(AllocError::PoolExhausted);
        };
        self.in_use.insert(slot);
        Ok(VmCidr::new(self.cidr_for_slot(slot)))
    }

    pub fn free(&mut self, cidr: VmCidr) {
        let slot = self.slot_for_cidr(cidr);
        if self.in_use.remove(&slot) {
            self.free.push(slot);
        }
    }

    fn cidr_for_slot(&self, slot: u32) -> Ipv4Addr {
        let raw = self.pool_base + slot * 4;
        Ipv4Addr::from(raw.to_be_bytes())
    }

    fn slot_for_cidr(&self, cidr: VmCidr) -> u32 {
        let raw = u32::from_be_bytes(cidr.network.octets());
        (raw - self.pool_base) / 4
    }

    pub fn live_count(&self) -> usize {
        self.in_use.len()
    }
}

/// Linux's `IFNAMSIZ` is 16 — including the trailing NUL. So
/// interface names are limited to 15 chars. `tap-engr-` is 9 chars,
/// leaving 6 for the sandbox ID prefix. UUIDs are unique enough at
/// 6 hex chars within a single host's lifetime.
pub fn tap_name_for(sandbox_id: SandboxId) -> String {
    let s = sandbox_id.to_string();
    // SandboxId stringifies as a UUID; take the first 6 hex chars.
    let prefix: String = s.chars().filter(|c| c.is_ascii_hexdigit()).take(6).collect();
    format!("tap-engr-{prefix}")
}

/// What the host-agent does to the per-VM iptables chain when the
/// final default rule is reached. `LogOnly` ships in this PR's
/// rollout; `Enforce` flips the switch once in-tree manifests
/// declare their `allow_hosts`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetPolicy {
    LogOnly,
    Enforce,
}

impl Default for NetPolicy {
    fn default() -> Self {
        Self::LogOnly
    }
}

impl NetPolicy {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "log_only" | "log-only" => Ok(Self::LogOnly),
            "enforce" => Ok(Self::Enforce),
            other => Err(format!(
                "unknown engram fc-net-policy `{other}` — expected log_only|enforce"
            )),
        }
    }
}

/// String-builder for the per-VM iptables ruleset. Render-only
/// (no shell-out) so it's unit-testable on macOS. The runtime
/// applies these by spawning `iptables` per line.
#[derive(Clone, Debug)]
pub struct ChainPlan {
    pub chain_name: String,
    pub vm_cidr: VmCidr,
    pub allow_ips: Vec<(Ipv4Addr, String)>,
    pub network: NetworkPolicy,
    pub host_policy: NetPolicy,
}

impl ChainPlan {
    pub fn new(
        sandbox_id: SandboxId,
        vm_cidr: VmCidr,
        network: NetworkPolicy,
        host_policy: NetPolicy,
    ) -> Self {
        Self {
            chain_name: chain_name_for(sandbox_id),
            vm_cidr,
            allow_ips: Vec::new(),
            network,
            host_policy,
        }
    }

    pub fn with_allow_ips(mut self, ips: Vec<(Ipv4Addr, String)>) -> Self {
        self.allow_ips = ips;
        self
    }

    /// Lines to apply at `create` time. Each line is a single
    /// `iptables ...` invocation (without the `iptables` binary
    /// prefix — the runtime adds that).
    pub fn create_lines(&self) -> Vec<String> {
        let comment = &self.chain_name;
        let cidr = self.vm_cidr.cidr_str();
        let host = self.vm_cidr.host();
        let mut out = Vec::new();

        // Build the per-VM chain.
        out.push(format!("-N {comment}"));

        // 1. Hard-isolation drops (host LAN protection).
        for net in HOST_LAN_BLOCK {
            out.push(format!(
                "-A {comment} -d {net} -j DROP -m comment --comment {comment}-lan",
            ));
        }

        // 2. DNS allow to public resolver.
        for proto in ["udp", "tcp"] {
            out.push(format!(
                "-A {comment} -p {proto} --dport 53 -d {dns} -j ACCEPT \
                 -m comment --comment {comment}-dns",
                dns = PUBLIC_DNS,
            ));
        }

        // 3. Resolved allow_hosts.
        for (ip, hostname) in &self.allow_ips {
            out.push(format!(
                "-A {comment} -d {ip} -j ACCEPT \
                 -m comment --comment {comment}-allow-{hostname}",
            ));
        }

        // 4. Final default.
        match (self.host_policy, self.network.default) {
            (NetPolicy::Enforce, NetworkDefault::Deny) => {
                out.push(format!(
                    "-A {comment} -j DROP -m comment --comment {comment}-deny",
                ));
            }
            (NetPolicy::LogOnly, NetworkDefault::Deny) => {
                // Surface what *would* have been blocked so
                // operators can see if the manifest is missing
                // entries before flipping to enforce. log-prefix has
                // no whitespace because the runtime applies rules by
                // splitting on whitespace; `:` keeps it greppable.
                out.push(format!(
                    "-A {comment} -j LOG --log-prefix {comment}:would-drop: \
                     -m comment --comment {comment}-log",
                ));
                out.push(format!(
                    "-A {comment} -j ACCEPT -m comment --comment {comment}-log-accept",
                ));
            }
            (_, NetworkDefault::Allow) => {
                out.push(format!(
                    "-A {comment} -j ACCEPT -m comment --comment {comment}-allow",
                ));
            }
        }

        // Wire FORWARD + INPUT + NAT.
        out.push(format!(
            "-I FORWARD -s {cidr} -j {comment} -m comment --comment {comment}",
        ));
        out.push(format!(
            "-I INPUT -s {cidr} -j DROP -m comment --comment {comment}-host-input",
        ));
        out.push(format!(
            "-I INPUT -s {cidr} -d {host} -p icmp -j ACCEPT \
             -m comment --comment {comment}-icmp",
        ));
        out.push(format!(
            "-t nat -A POSTROUTING -s {cidr} ! -d {pool} -j MASQUERADE \
             -m comment --comment {comment}-masq",
            pool = ENGRAM_POOL_CIDR,
        ));

        out
    }

    /// Lines to apply at `destroy` time. Reverses each insert and
    /// flushes/deletes the per-VM chain.
    pub fn destroy_lines(&self) -> Vec<String> {
        let comment = &self.chain_name;
        let cidr = self.vm_cidr.cidr_str();
        let host = self.vm_cidr.host();
        vec![
            format!(
                "-t nat -D POSTROUTING -s {cidr} ! -d {pool} -j MASQUERADE \
                 -m comment --comment {comment}-masq",
                pool = ENGRAM_POOL_CIDR,
            ),
            format!(
                "-D INPUT -s {cidr} -d {host} -p icmp -j ACCEPT \
                 -m comment --comment {comment}-icmp",
            ),
            format!(
                "-D INPUT -s {cidr} -j DROP -m comment --comment {comment}-host-input",
            ),
            format!(
                "-D FORWARD -s {cidr} -j {comment} -m comment --comment {comment}",
            ),
            format!("-F {comment}"),
            format!("-X {comment}"),
        ]
    }
}

/// Lines applied once at host-agent startup. Idempotent — operators
/// who restart the coord don't double up the rules. The runtime
/// dedupes by checking for the comment tag before inserting.
pub fn host_startup_lines() -> Vec<String> {
    vec![format!(
        "-I FORWARD 1 -s {pool} -d {pool} -j DROP \
         -m comment --comment engram-isolate-vm-vm",
        pool = ENGRAM_POOL_CIDR,
    )]
}

fn chain_name_for(sandbox_id: SandboxId) -> String {
    let s = sandbox_id.to_string();
    // Take 12 hex chars — 64 bits of entropy is plenty for chain
    // uniqueness inside a single host. iptables chain names are
    // capped at 28 chars; "engram-sb-" is 10, leaving 18.
    let prefix: String = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(12)
        .collect();
    format!("engram-sb-{prefix}")
}

/// Default engram CIDR. Operators override via
/// `--fc-net-cidr 10.201.0.0/16` if they're already using
/// `10.200.0.0/16` for something else.
pub const ENGRAM_POOL_CIDR: &str = "10.200.0.0/16";

const PUBLIC_DNS: &str = "1.1.1.1";

const HOST_LAN_BLOCK: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
    "127.0.0.0/8",
];

/// Per-sandbox network state stashed on `LiveSandbox` so destroy
/// can release the slot and tear down the host-side rules.
#[derive(Clone, Debug)]
pub struct NetSetup {
    pub vm_cidr: VmCidr,
    pub tap_name: String,
    pub plan: ChainPlan,
}

/// Errors from the Linux runtime layer. Distinct from `SandboxError`
/// so the caller decides whether to escalate (`create` failure) or
/// just log (`destroy` best-effort cleanup).
#[derive(Debug)]
pub enum NetError {
    Spawn(String, std::io::Error),
    Failed { cmd: String, status: i32, stderr: String },
    Resolve(String, std::io::Error),
    Alloc(AllocError),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(cmd, e) => write!(f, "spawn {cmd}: {e}"),
            Self::Failed { cmd, status, stderr } => {
                write!(f, "{cmd} exited with status {status}: {stderr}")
            }
            Self::Resolve(host, e) => write!(f, "resolve {host}: {e}"),
            Self::Alloc(e) => write!(f, "alloc: {e}"),
        }
    }
}

impl std::error::Error for NetError {}

impl From<NetError> for engram_core::SandboxError {
    fn from(e: NetError) -> Self {
        engram_core::SandboxError::Vm(Box::new(StringError(e.to_string())))
    }
}

#[derive(Debug)]
struct StringError(String);
impl std::fmt::Display for StringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for StringError {}

/// Resolve each `manifest.network.allow_hosts` entry to its current
/// IPv4s. Hostnames are paired with `:443` for the lookup (resolver
/// requires a port even if we discard it). Failures don't fail the
/// sandbox create — we log + skip the entry, leaving its IP set
/// empty (the egress refresher's next tick will retry).
pub async fn resolve_allow_hosts(hosts: &[String]) -> Vec<(Ipv4Addr, String)> {
    let mut out = Vec::new();
    for host in hosts {
        let target = format!("{host}:443");
        let result = tokio::net::lookup_host(target).await;
        match result {
            Ok(addrs) => {
                for addr in addrs {
                    if let std::net::IpAddr::V4(v4) = addr.ip() {
                        out.push((v4, host.clone()));
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    %host,
                    error = %e,
                    "allow_hosts resolve failed; entry will be retried by the egress refresher",
                );
            }
        }
    }
    out
}

/// Run a single `iptables`/`ip` command. Stdout discarded; stderr
/// captured for error reporting. The runtime spawns these one at a
/// time — the command set is small enough that batching isn't worth
/// the complexity (and `iptables-restore` would lock for everyone).
#[cfg(target_os = "linux")]
async fn run_cmd(bin: &str, args: &[&str]) -> Result<(), NetError> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args);
    let output = cmd
        .output()
        .await
        .map_err(|e| NetError::Spawn(bin.to_string(), e))?;
    if !output.status.success() {
        return Err(NetError::Failed {
            cmd: format!("{bin} {}", args.join(" ")),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

/// Run an iptables rule line — split on whitespace and exec.
/// `engram-sb-...` chain names and dotted-quad IPs are
/// whitespace-free, so split-on-space is safe for the rule shapes
/// `ChainPlan` produces.
#[cfg(target_os = "linux")]
async fn run_iptables(line: &str) -> Result<(), NetError> {
    let argv: Vec<&str> = line.split_whitespace().collect();
    run_cmd("iptables", &argv).await
}

/// Apply once-per-host startup rules + enable IP forwarding.
/// Idempotent: each rule has a unique `--comment` tag, and we
/// check-then-insert so a coord restart doesn't double up.
#[cfg(target_os = "linux")]
pub async fn host_startup() -> Result<(), NetError> {
    // Enable forwarding via /proc/sys (kernel-level toggle).
    if let Err(e) =
        tokio::fs::write("/proc/sys/net/ipv4/ip_forward", b"1").await
    {
        return Err(NetError::Spawn(
            "write /proc/sys/net/ipv4/ip_forward".into(),
            e,
        ));
    }

    for line in host_startup_lines() {
        // Try a check first (`-C`) — if the rule exists, skip insert.
        let check_argv: Vec<String> = line
            .replace("-I FORWARD 1", "-C FORWARD")
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let check_argv_ref: Vec<&str> = check_argv.iter().map(|s| s.as_str()).collect();
        let exists = run_cmd("iptables", &check_argv_ref).await.is_ok();
        if !exists {
            run_iptables(&line).await?;
        }
    }
    Ok(())
}

/// Provision the host-side networking for a fresh sandbox: alloc
/// /30, create TAP, assign gateway IP, build per-VM iptables chain.
/// Returns the [`NetSetup`] the caller stashes on `LiveSandbox`.
#[cfg(target_os = "linux")]
pub async fn provision(
    sandbox_id: SandboxId,
    allocator: &parking_lot::Mutex<NetworkAllocator>,
    network: NetworkPolicy,
    host_policy: NetPolicy,
) -> Result<NetSetup, NetError> {
    let vm_cidr = allocator
        .lock()
        .alloc()
        .map_err(NetError::Alloc)?;
    let tap_name = tap_name_for(sandbox_id);
    let host_addr = format!("{}/30", vm_cidr.host());

    // Create the TAP. `tuntap add ... mode tap` is idempotent on
    // first failure (returns non-zero if the device already exists),
    // so we delete first to be safe — leftovers from a previous
    // crashed sandbox would otherwise wedge create.
    let _ = run_cmd("ip", &["link", "delete", &tap_name]).await;
    run_cmd("ip", &["tuntap", "add", &tap_name, "mode", "tap"]).await?;
    run_cmd("ip", &["addr", "add", &host_addr, "dev", &tap_name]).await?;
    run_cmd("ip", &["link", "set", "dev", &tap_name, "up"]).await?;

    // Resolve allow_hosts now so the chain gets ACCEPT rules for
    // current IPs. The egress refresher's job is keeping these
    // honest as DNS rotates.
    let allow_ips = resolve_allow_hosts(&network.allow_hosts).await;
    let plan = ChainPlan::new(sandbox_id, vm_cidr, network, host_policy)
        .with_allow_ips(allow_ips);
    for line in plan.create_lines() {
        run_iptables(&line).await?;
    }

    Ok(NetSetup {
        vm_cidr,
        tap_name,
        plan,
    })
}

/// Tear down the host-side networking. Best-effort: each step's
/// failure logs but doesn't stop the rest, since the VM is dead and
/// we want to release as many resources as possible.
#[cfg(target_os = "linux")]
pub async fn teardown(
    setup: &NetSetup,
    allocator: &parking_lot::Mutex<NetworkAllocator>,
) {
    for line in setup.plan.destroy_lines() {
        if let Err(e) = run_iptables(&line).await {
            tracing::debug!(rule = %line, error = %e, "iptables teardown rule failed");
        }
    }
    if let Err(e) = run_cmd("ip", &["link", "delete", &setup.tap_name]).await {
        tracing::debug!(tap = %setup.tap_name, error = %e, "tap delete failed");
    }
    allocator.lock().free(setup.vm_cidr);
}

// Non-Linux stubs so the rest of the crate compiles cross-platform
// (macOS dev — the FC backend impl itself is also Linux-gated, but
// keeping these as plain `unimplemented!` would surface in
// `cargo check --workspace` runs on Mac).
#[cfg(not(target_os = "linux"))]
pub async fn host_startup() -> Result<(), NetError> {
    Err(NetError::Spawn(
        "host_startup".into(),
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "FC networking is Linux-only",
        ),
    ))
}

#[cfg(not(target_os = "linux"))]
pub async fn provision(
    _sandbox_id: SandboxId,
    _allocator: &parking_lot::Mutex<NetworkAllocator>,
    _network: NetworkPolicy,
    _host_policy: NetPolicy,
) -> Result<NetSetup, NetError> {
    Err(NetError::Spawn(
        "provision".into(),
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "FC networking is Linux-only",
        ),
    ))
}

#[cfg(not(target_os = "linux"))]
pub async fn teardown(_setup: &NetSetup, _allocator: &parking_lot::Mutex<NetworkAllocator>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn id() -> SandboxId {
        SandboxId::new()
    }

    #[test]
    fn vm_cidr_host_and_guest_are_consecutive() {
        let c = VmCidr::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        assert_eq!(c.host(), Ipv4Addr::from_str("10.200.0.1").unwrap());
        assert_eq!(c.guest(), Ipv4Addr::from_str("10.200.0.2").unwrap());
        assert_eq!(c.cidr_str(), "10.200.0.0/30");
    }

    #[test]
    fn kernel_ip_arg_is_the_static_form() {
        let c = VmCidr::new(Ipv4Addr::from_str("10.200.0.4").unwrap());
        assert_eq!(
            c.kernel_ip_arg(),
            "ip=10.200.0.6::10.200.0.5:255.255.255.252::eth0:off",
        );
    }

    #[test]
    fn allocator_carves_unique_30s_in_order() {
        let mut a = NetworkAllocator::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        let c0 = a.alloc().unwrap();
        let c1 = a.alloc().unwrap();
        let c2 = a.alloc().unwrap();
        assert_eq!(c0.cidr_str(), "10.200.0.0/30");
        assert_eq!(c1.cidr_str(), "10.200.0.4/30");
        assert_eq!(c2.cidr_str(), "10.200.0.8/30");
        assert_eq!(a.live_count(), 3);
    }

    #[test]
    fn allocator_recycles_freed_slots() {
        let mut a = NetworkAllocator::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        let c0 = a.alloc().unwrap();
        let _c1 = a.alloc().unwrap();
        a.free(c0);
        let reused = a.alloc().unwrap();
        assert_eq!(reused.cidr_str(), "10.200.0.0/30");
    }

    #[test]
    fn allocator_pool_size_is_16384() {
        let a = NetworkAllocator::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        assert_eq!(a.pool_size, 16384);
    }

    #[test]
    fn tap_name_fits_in_ifnamsiz() {
        let n = tap_name_for(id());
        assert!(n.len() <= 15, "tap name {n} too long for IFNAMSIZ");
        assert!(n.starts_with("tap-engr-"));
    }

    #[test]
    fn chain_plan_renders_default_deny_under_enforce() {
        let cidr = VmCidr::new(Ipv4Addr::from_str("10.200.4.0").unwrap());
        let plan = ChainPlan::new(id(), cidr, NetworkPolicy::default(), NetPolicy::Enforce);
        let rendered = plan.create_lines().join("\n");
        // Hard-isolation rules.
        assert!(rendered.contains("-d 10.0.0.0/8 -j DROP"));
        assert!(rendered.contains("-d 192.168.0.0/16 -j DROP"));
        // DNS allow.
        assert!(rendered.contains("-p udp --dport 53 -d 1.1.1.1 -j ACCEPT"));
        // Final deny.
        assert!(rendered.contains("-deny"));
        assert!(!rendered.contains("would-drop"));
        // FORWARD wire-up + MASQUERADE.
        assert!(rendered.contains("-I FORWARD -s 10.200.4.0/30"));
        assert!(rendered.contains("MASQUERADE"));
    }

    #[test]
    fn chain_plan_renders_log_only_under_log_policy() {
        let cidr = VmCidr::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        let plan = ChainPlan::new(id(), cidr, NetworkPolicy::default(), NetPolicy::LogOnly);
        let rendered = plan.create_lines().join("\n");
        assert!(rendered.contains("would-drop"));
        // log-prefix must not contain whitespace — the runtime
        // splits on space, and a quoted `--log-prefix "x y "` would
        // be parsed as multiple args by iptables.
        assert!(!rendered.contains("would-drop "));
        assert!(rendered.contains("-log-accept"));
        // No final DROP under log_only.
        assert!(!rendered.contains("-deny"));
    }

    #[test]
    fn chain_plan_includes_resolved_allow_hosts() {
        let cidr = VmCidr::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        let mut policy = NetworkPolicy::default();
        policy.allow_hosts = vec!["api.github.com".into()];
        let plan = ChainPlan::new(id(), cidr, policy, NetPolicy::Enforce).with_allow_ips(vec![
            (Ipv4Addr::from_str("140.82.114.6").unwrap(), "api.github.com".into()),
        ]);
        let rendered = plan.create_lines().join("\n");
        assert!(rendered.contains("-d 140.82.114.6 -j ACCEPT"));
        assert!(rendered.contains("-allow-api.github.com"));
    }

    #[test]
    fn chain_plan_destroy_lines_undo_create_lines() {
        let cidr = VmCidr::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        let plan = ChainPlan::new(id(), cidr, NetworkPolicy::default(), NetPolicy::Enforce);
        let destroy = plan.destroy_lines().join("\n");
        // Symmetric -D for FORWARD/INPUT/POSTROUTING; -F + -X to
        // tear down the chain itself.
        assert!(destroy.contains("-D FORWARD"));
        assert!(destroy.contains("-D INPUT"));
        assert!(destroy.contains("-D POSTROUTING"));
        assert!(destroy.contains("-F engram-sb-"));
        assert!(destroy.contains("-X engram-sb-"));
    }

    #[test]
    fn host_startup_lines_block_inter_vm() {
        let lines = host_startup_lines().join("\n");
        assert!(lines.contains("-s 10.200.0.0/16 -d 10.200.0.0/16 -j DROP"));
        assert!(lines.contains("engram-isolate-vm-vm"));
    }

    #[test]
    fn net_policy_parses_both_forms() {
        assert_eq!(NetPolicy::parse("enforce"), Ok(NetPolicy::Enforce));
        assert_eq!(NetPolicy::parse("log_only"), Ok(NetPolicy::LogOnly));
        assert_eq!(NetPolicy::parse("log-only"), Ok(NetPolicy::LogOnly));
        assert!(NetPolicy::parse("strict").is_err());
    }
}
