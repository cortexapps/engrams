//! Per-sandbox networking for the Firecracker backend.
//!
//! Each VM gets a dedicated `/30` carved out of the host-agent's
//! configured engram CIDR (default `10.200.0.0/16`). The host owns
//! the gateway IP; the guest gets the next address. A TAP device
//! lives on the host, terminated at the gateway IP. Static IP via
//! the kernel `ip=` cmdline (`CONFIG_IP_PNP_*=y`) — no DHCP server
//! on the host, less attack surface.
//!
//! **Policy enforcement lives at the egress proxy, not iptables.**
//! Earlier iterations applied a per-VM iptables chain with the
//! `manifest.network.allow_hosts` list resolved to IPs; that
//! suffered from DNS re-resolve drift and duplicated the secret
//! broker's hostname allowlist at a different layer (L3 vs L7).
//! The proxy is now the single source of truth: iptables shrinks to
//! a static ruleset applied once at host-agent startup that drops
//! everything except VM→proxy and VM→DNS, plus standard hard-isolation
//! drops (RFC1918, link-local, loopback, inter-VM). Per-VM
//! provisioning is now just TAP creation.

use std::collections::HashSet;
use std::net::Ipv4Addr;

use engram_core::types::ids::SandboxId;

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
/// leaving 6 for the sandbox ID prefix.
pub fn tap_name_for(sandbox_id: SandboxId) -> String {
    let s = sandbox_id.to_string();
    let prefix: String = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(6)
        .collect();
    format!("tap-engr-{prefix}")
}

/// Static iptables ruleset applied once per host-agent at startup.
/// All policy lives at the proxy now; iptables is hard isolation +
/// proxy-redirect only.
///
/// `proxy_port`: when Some, REDIRECT VM→tcp/443 to that port (the
/// proxy runs on `127.0.0.1:port` per the coord wiring) AND apply a
/// final FORWARD DROP so the proxy is the only egress path. When
/// None (test/dev mode), VM egress to the public internet is
/// allowed under the standard MASQUERADE; only host-LAN and
/// inter-VM traffic is dropped.
pub fn host_startup_lines(proxy_port: Option<u16>) -> Vec<String> {
    let pool = ENGRAM_POOL_CIDR;
    let mut out = Vec::new();

    // 1. Inter-VM block — once. (-I prepends so this beats anything
    //    distro-installed.)
    out.push(format!(
        "-I FORWARD 1 -s {pool} -d {pool} -j DROP \
         -m comment --comment engram-isolate-vm-vm",
    ));

    // 2. Host-LAN protection: VMs can't reach RFC1918 / link-local
    //    / loopback (the host's own private networks are off-limits).
    for net in HOST_LAN_BLOCK {
        out.push(format!(
            "-A FORWARD -s {pool} -d {net} -j DROP \
             -m comment --comment engram-host-lan",
        ));
    }

    // 3. host-INPUT protection: VMs can't reach the host directly
    //    (no DNS server bound on the gateway, no SSH, no coord HTTP).
    //    EXCEPT: when proxy mode is on, the iptables REDIRECT in
    //    PREROUTING rewrites the destination IP to localhost; the
    //    rewritten packet still has the VM's source IP and hits the
    //    INPUT chain on its way to the proxy's listening socket. So
    //    we need an explicit ACCEPT for the proxy port before the
    //    blanket DROP. Order matters: ACCEPT first, DROP after.
    if let Some(port) = proxy_port {
        out.push(format!(
            "-A INPUT -s {pool} -p tcp --dport {port} -j ACCEPT \
             -m comment --comment engram-proxy-input",
        ));
    }
    out.push(format!(
        "-A INPUT -s {pool} -j DROP -m comment --comment engram-host-input",
    ));

    // 4. DNS allow to public resolver. The guest's resolv.conf points
    //    at 1.1.1.1; the proxy resolves hostnames itself, so DNS
    //    inside the VM only happens for app-level lookups.
    out.push(format!(
        "-A FORWARD -s {pool} -p udp --dport 53 -d {PUBLIC_DNS} \
         -j ACCEPT -m comment --comment engram-dns",
    ));
    out.push(format!(
        "-A FORWARD -s {pool} -p tcp --dport 53 -d {PUBLIC_DNS} \
         -j ACCEPT -m comment --comment engram-dns",
    ));

    if let Some(port) = proxy_port {
        // 5a. PROXY mode: REDIRECT VM→tcp/443 to the local proxy
        //     port. -t nat -A PREROUTING with -i tap-engr-+ matches
        //     any of our TAPs (the `+` is iptables' wildcard).
        out.push(format!(
            "-t nat -A PREROUTING -i tap-engr-+ -p tcp --dport 443 \
             -j REDIRECT --to-port {port} \
             -m comment --comment engram-proxy-redirect",
        ));
        // 5b. After the REDIRECT, the connection is destined for
        //     the host's local socket; iptables FORWARD doesn't
        //     see it. So we don't need a separate ACCEPT for that
        //     traffic. We DO still want to forbid any *other*
        //     egress: drop everything else from the pool.
        out.push(format!(
            "-A FORWARD -s {pool} -j DROP \
             -m comment --comment engram-default-deny",
        ));
    } else {
        // 5c. NO-PROXY mode: allow non-LAN egress (the host-LAN
        //     drops above already filtered). MASQUERADE is what
        //     makes return traffic come back to the right VM.
    }

    // 6. MASQUERADE on POSTROUTING — required regardless of proxy
    //    mode for return traffic to reach the VM.
    out.push(format!(
        "-t nat -A POSTROUTING -s {pool} ! -d {pool} -j MASQUERADE \
         -m comment --comment engram-masq",
    ));

    out
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
/// can release the slot and delete the TAP. No iptables per-VM —
/// all policy is global (applied once at host_startup) or at the
/// proxy.
#[derive(Clone, Debug)]
pub struct NetSetup {
    pub vm_cidr: VmCidr,
    pub tap_name: String,
}

/// Errors from the Linux runtime layer. Distinct from `SandboxError`
/// so the caller decides whether to escalate (`create` failure) or
/// just log (`destroy` best-effort cleanup).
#[derive(Debug)]
pub enum NetError {
    Spawn(String, std::io::Error),
    Failed {
        cmd: String,
        status: i32,
        stderr: String,
    },
    Alloc(AllocError),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(cmd, e) => write!(f, "spawn {cmd}: {e}"),
            Self::Failed {
                cmd,
                status,
                stderr,
            } => {
                write!(f, "{cmd} exited with status {status}: {stderr}")
            }
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

#[cfg(target_os = "linux")]
async fn run_iptables(line: &str) -> Result<(), NetError> {
    let argv: Vec<&str> = line.split_whitespace().collect();
    run_cmd("iptables", &argv).await
}

/// Apply the static ruleset. Idempotent — each rule has a unique
/// `--comment` tag and we check-then-insert so a coord restart
/// doesn't double up.
#[cfg(target_os = "linux")]
pub async fn host_startup(proxy_port: Option<u16>) -> Result<(), NetError> {
    if let Err(e) = tokio::fs::write("/proc/sys/net/ipv4/ip_forward", b"1").await {
        return Err(NetError::Spawn(
            "write /proc/sys/net/ipv4/ip_forward".into(),
            e,
        ));
    }
    // Required when proxy mode is on: iptables PREROUTING REDIRECT
    // rewrites a guest-bound 1.2.3.4 destination to 127.0.0.1 (the
    // proxy's listener). Without `route_localnet`, the kernel marks
    // any 127.0.0.0/8 destination arriving on a non-loopback
    // interface as a martian and drops it before INPUT delivery.
    // Setting `all` covers all current and future TAPs without
    // having to set it per-interface.
    if proxy_port.is_some() {
        if let Err(e) = tokio::fs::write("/proc/sys/net/ipv4/conf/all/route_localnet", b"1").await {
            return Err(NetError::Spawn(
                "write /proc/sys/net/ipv4/conf/all/route_localnet".into(),
                e,
            ));
        }
    }
    for line in host_startup_lines(proxy_port) {
        // Idempotency check: replace the leading `-A`/`-I` with `-C`
        // (or skip altogether for non-rule meta commands like
        // create-chain — none of our lines do that today). Order:
        // `-I FORWARD 1` and `-I INPUT` need a check, NAT-table
        // inserts need `-t nat -C`. We let iptables tell us via its
        // exit code: success on -C means "exists, skip insert."
        let normalized = check_form(&line);
        let argv: Vec<&str> = normalized.split_whitespace().collect();
        let exists = run_cmd("iptables", &argv).await.is_ok();
        if !exists {
            run_iptables(&line).await?;
        }
    }
    Ok(())
}

/// Convert an `-I/-A`-style line into its `-C` (check) twin. We do
/// the swap textually to avoid duplicating the rule construction.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn check_form(line: &str) -> String {
    line.replacen("-I FORWARD 1", "-C FORWARD", 1)
        .replacen("-A FORWARD", "-C FORWARD", 1)
        .replacen("-A INPUT", "-C INPUT", 1)
        .replacen("-t nat -A PREROUTING", "-t nat -C PREROUTING", 1)
        .replacen("-t nat -A POSTROUTING", "-t nat -C POSTROUTING", 1)
}

/// Provision the host-side networking for a fresh sandbox: alloc
/// a /30, create the TAP, assign the gateway IP, bring it up.
/// All policy is global; this is just the wire.
#[cfg(target_os = "linux")]
pub async fn provision(
    sandbox_id: SandboxId,
    allocator: &parking_lot::Mutex<NetworkAllocator>,
) -> Result<NetSetup, NetError> {
    let vm_cidr = allocator.lock().alloc().map_err(NetError::Alloc)?;
    let tap_name = tap_name_for(sandbox_id);
    let host_addr = format!("{}/30", vm_cidr.host());

    let _ = run_cmd("ip", &["link", "delete", &tap_name]).await;
    run_cmd("ip", &["tuntap", "add", &tap_name, "mode", "tap"]).await?;
    run_cmd("ip", &["addr", "add", &host_addr, "dev", &tap_name]).await?;
    run_cmd("ip", &["link", "set", "dev", &tap_name, "up"]).await?;

    Ok(NetSetup { vm_cidr, tap_name })
}

/// Tear down the host-side networking. Best-effort.
#[cfg(target_os = "linux")]
pub async fn teardown(setup: &NetSetup, allocator: &parking_lot::Mutex<NetworkAllocator>) {
    if let Err(e) = run_cmd("ip", &["link", "delete", &setup.tap_name]).await {
        tracing::debug!(tap = %setup.tap_name, error = %e, "tap delete failed");
    }
    allocator.lock().free(setup.vm_cidr);
}

// Non-Linux stubs.
#[cfg(not(target_os = "linux"))]
pub async fn host_startup(_proxy_port: Option<u16>) -> Result<(), NetError> {
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
    fn tap_name_fits_in_ifnamsiz() {
        let n = tap_name_for(id());
        assert!(n.len() <= 15, "tap name {n} too long for IFNAMSIZ");
        assert!(n.starts_with("tap-engr-"));
    }

    #[test]
    fn host_startup_no_proxy_drops_lan_but_allows_internet() {
        let lines = host_startup_lines(None).join("\n");
        // Inter-VM block.
        assert!(lines.contains("-s 10.200.0.0/16 -d 10.200.0.0/16 -j DROP"));
        // Host-LAN drops.
        assert!(lines.contains("-d 10.0.0.0/8 -j DROP"));
        assert!(lines.contains("-d 192.168.0.0/16 -j DROP"));
        // DNS allow.
        assert!(lines.contains("--dport 53 -d 1.1.1.1"));
        // No proxy redirect.
        assert!(!lines.contains("REDIRECT"));
        // No final default-deny — internet egress is open.
        assert!(!lines.contains("engram-default-deny"));
        // MASQUERADE present so return traffic reaches the VM.
        assert!(lines.contains("MASQUERADE"));
    }

    #[test]
    fn host_startup_with_proxy_redirects_443_and_default_denies() {
        let lines = host_startup_lines(Some(9443)).join("\n");
        assert!(lines.contains("-i tap-engr-+ -p tcp --dport 443"));
        assert!(lines.contains("--to-port 9443"));
        assert!(lines.contains("engram-default-deny"));
        assert!(lines.contains("--dport 9443 -j ACCEPT"));
        assert!(lines.contains("engram-proxy-input"));
    }

    #[test]
    fn host_startup_proxy_input_accept_comes_before_drop() {
        // Ordering matters — ACCEPT before DROP — otherwise the
        // blanket VM-INPUT drop catches the REDIRECTed proxy
        // traffic too. The two sit consecutively in the output;
        // assert ACCEPT line index < DROP line index.
        let lines = host_startup_lines(Some(9443));
        let accept_idx = lines.iter().position(|l| l.contains("engram-proxy-input"));
        let drop_idx = lines.iter().position(|l| l.contains("engram-host-input"));
        assert!(accept_idx.is_some() && drop_idx.is_some());
        assert!(accept_idx < drop_idx);
    }

    #[test]
    fn check_form_swaps_insert_for_check() {
        assert_eq!(
            check_form("-I FORWARD 1 -s 10.200.0.0/16 -d 10.200.0.0/16 -j DROP"),
            "-C FORWARD -s 10.200.0.0/16 -d 10.200.0.0/16 -j DROP",
        );
        assert_eq!(
            check_form("-A INPUT -s 10.200.0.0/16 -j DROP"),
            "-C INPUT -s 10.200.0.0/16 -j DROP",
        );
        assert_eq!(
            check_form("-t nat -A POSTROUTING -s 10.200.0.0/16 -j MASQUERADE"),
            "-t nat -C POSTROUTING -s 10.200.0.0/16 -j MASQUERADE",
        );
    }
}
