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

    /// `.0` of the /30. Used by the snapshot manifest to record which
    /// slot the source VM had so restore can re-reserve it.
    pub fn network(&self) -> Ipv4Addr {
        self.network
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
    /// `reserve()` asked for a slot that's already handed out.
    /// Cross-host cold resume can hit this when the receiving host's
    /// allocator already gave the same /30 to another sandbox.
    SlotTaken,
    /// `reserve()` asked for a /30 that doesn't fall within this
    /// allocator's pool (different /16, or alignment off). Treated as
    /// a hard error rather than skip-with-warn — the manifest must
    /// have been written by a host configured against a different
    /// pool, which is a deployment bug, not a routine collision.
    OutOfPool,
}

impl std::fmt::Display for AllocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PoolExhausted => write!(f, "engram CIDR pool exhausted"),
            Self::SlotTaken => write!(f, "/30 slot already in use"),
            Self::OutOfPool => write!(f, "/30 outside the engram pool"),
        }
    }
}

impl std::error::Error for AllocError {}

impl NetworkAllocator {
    /// Build an allocator over `pool/16`. Lower octets must be 0
    /// (we carve exactly one /16 — `10.200.0.0/16` is the canonical
    /// default).
    ///
    /// ADR 0014 M1.16: slot 0 (the `10.200.0.0/30` at the base of
    /// the pool) is **reserved**. That slot is the bake-time CIDR
    /// every warm-restored sandbox's snapshot embeds — the host
    /// recreates a TAP at `10.200.0.1` inside the per-VM netns
    /// to match the VM's baked-in default gateway. If `alloc()`
    /// also handed slot 0 out as a SNAT slot, the veth-B inside
    /// the netns would get assigned `10.200.0.2`, colliding with
    /// the VM's eth0. Kernel sees `10.200.0.2` as locally hosted
    /// and short-circuits host-agent dials to ttyd through
    /// loopback → SHELL tab fails with EHOSTUNREACH/ECONNREFUSED.
    /// Observed in prod on session d20c4715 (2026-05-20).
    ///
    /// Reserving slot 0 at construction sidesteps the collision —
    /// SNAT slots start at slot 1 (`10.200.0.4/30`), guaranteed
    /// disjoint from `bake_cidr`. Cold-path sandboxes also avoid
    /// it for free (their /30 comes from the same allocator).
    pub fn new(pool: Ipv4Addr) -> Self {
        let octets = pool.octets();
        let base = u32::from_be_bytes([octets[0], octets[1], 0, 0]);
        let mut in_use = HashSet::new();
        // Reserve slot 0 (the bake CIDR). See ADR 0014 M1.16 above.
        in_use.insert(0);
        Self {
            pool_base: base,
            // 16384 /30s in a /16.
            pool_size: 1 << 14,
            // Start at slot 1; slot 0 is the bake CIDR and stays
            // permanently in `in_use`.
            next: 1,
            in_use,
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
        let Some(slot) = self.try_slot_for_cidr(cidr) else {
            return;
        };
        // ADR 0014 M1.16: slot 0 (the bake CIDR) is permanently
        // reserved — silently refuse to free it so a stray
        // `allocator.free(bake_cidr)` doesn't accidentally release
        // the slot back into the alloc pool and reintroduce the
        // SNAT/eth0 collision.
        if slot == 0 {
            return;
        }
        if self.in_use.remove(&slot) {
            self.free.push(slot);
        }
    }

    /// Re-allocate a specific /30 — the path restore takes after the
    /// snapshot manifest tells it which slot the original VM had. The
    /// kernel's `ip=` cmdline is baked into `state.bin`, so a restored
    /// VM has to come back on the same guest IP, which means the same
    /// /30. Cross-host: if the receiving host happens to have already
    /// handed out this slot, the caller falls back to no-egress
    /// rather than failing the restore.
    pub fn reserve(&mut self, cidr: VmCidr) -> Result<VmCidr, AllocError> {
        let slot = self.try_slot_for_cidr(cidr).ok_or(AllocError::OutOfPool)?;
        if slot >= self.pool_size {
            return Err(AllocError::OutOfPool);
        }
        if self.in_use.contains(&slot) {
            return Err(AllocError::SlotTaken);
        }
        if let Some(pos) = self.free.iter().position(|&s| s == slot) {
            self.free.swap_remove(pos);
        } else if slot >= self.next {
            // Bump `next` past the requested slot. Mark intervening
            // slots as free so a subsequent `alloc()` doesn't skip
            // them and prematurely exhaust the pool.
            for s in self.next..slot {
                self.free.push(s);
            }
            self.next = slot + 1;
        }
        self.in_use.insert(slot);
        Ok(cidr)
    }

    fn cidr_for_slot(&self, slot: u32) -> Ipv4Addr {
        let raw = self.pool_base + slot * 4;
        Ipv4Addr::from(raw.to_be_bytes())
    }

    /// Inverse of `cidr_for_slot`. Returns None when the CIDR is
    /// outside the pool or misaligned to a /30 boundary, so a stale
    /// manifest can't crash the host with subtraction underflow.
    fn try_slot_for_cidr(&self, cidr: VmCidr) -> Option<u32> {
        let raw = u32::from_be_bytes(cidr.network.octets());
        if raw < self.pool_base {
            return None;
        }
        let off = raw - self.pool_base;
        if off & 0b11 != 0 {
            return None;
        }
        Some(off / 4)
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
///
/// `dns_port`: when proxy mode is on, where the filtering DNS proxy
/// is bound. Iptables REDIRECTs guest `{udp,tcp}/53` to this port
/// so the proxy can enforce `allow_hosts` on resolution. Defaults to
/// 5353 if the caller passes `None` while proxy mode is on —
/// matching the default in `engram-egress-proxy::ProxyConfig::new`
/// (chosen to avoid the systemd-resolved bind on 127.0.0.53:53).
pub fn host_startup_lines(proxy_port: Option<u16>, dns_port: Option<u16>) -> Vec<String> {
    let dns_port = dns_port.unwrap_or(DEFAULT_DNS_PORT);
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
    //    (no SSH, no coord HTTP). EXCEPT: when proxy mode is on,
    //    the PREROUTING REDIRECT rewrites the destination IP to
    //    localhost; the rewritten packet still has the VM's source
    //    IP and hits the INPUT chain on its way to the proxy's
    //    listening sockets. So we need explicit ACCEPTs for the
    //    proxy's TCP/443 port AND its DNS port (53, both udp + tcp)
    //    before the blanket DROP. Order matters: ACCEPT first,
    //    DROP after.
    //
    //    ADR 0014 issue #6 follow-up: the host-agent's ProxyShell
    //    tunnel dials the in-guest ttyd from host root (cold path)
    //    or from the per-VM netns (warm path). Either way, the
    //    guest's SYN+ACK reply comes back to host root with
    //    src=10.200.0.x dst=10.200.0.1, hits the INPUT chain, and
    //    the blanket DROP below catches it (no specific dport ACCEPT
    //    matches the host's ephemeral source port). Result: every
    //    host→VM TCP dial hangs until ETIMEDOUT and the SHELL tab
    //    fails. Fix: accept ESTABLISHED + RELATED return traffic
    //    from the pool BEFORE the DROP. This only permits return
    //    traffic for flows the host initiated — it does NOT widen
    //    the VM→host attack surface (new VM-initiated flows still
    //    hit the DROP). Required for ProxyShell to actually reach
    //    ttyd; observed in prod (session 379abfec) post-M1.16.
    out.push(format!(
        "-A INPUT -s {pool} -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT \
         -m comment --comment engram-host-input-established",
    ));
    if let Some(port) = proxy_port {
        out.push(format!(
            "-A INPUT -s {pool} -p tcp --dport {port} -j ACCEPT \
             -m comment --comment engram-proxy-input",
        ));
        out.push(format!(
            "-A INPUT -s {pool} -p udp --dport {dns_port} -j ACCEPT \
             -m comment --comment engram-proxy-dns-input",
        ));
        out.push(format!(
            "-A INPUT -s {pool} -p tcp --dport {dns_port} -j ACCEPT \
             -m comment --comment engram-proxy-dns-input",
        ));
    }
    out.push(format!(
        "-A INPUT -s {pool} -j DROP -m comment --comment engram-host-input",
    ));

    if let Some(port) = proxy_port {
        // 4a. PROXY mode: REDIRECT VM→tcp/443 to the local proxy
        //     port. -t nat -A PREROUTING with -i tap-engr-+ matches
        //     any of our TAPs (the `+` is iptables' wildcard).
        out.push(format!(
            "-t nat -A PREROUTING -i tap-engr-+ -p tcp --dport 443 \
             -j REDIRECT --to-port {port} \
             -m comment --comment engram-proxy-redirect",
        ));
        // 4b. REDIRECT VM DNS (both transports) to the local DNS
        //     proxy on :53. This catches queries to ANY upstream IP
        //     the guest tries — 1.1.1.1, 8.8.8.8, even attacker-
        //     controlled — and routes them through the filtering
        //     proxy. The proxy then enforces `allow_hosts` on the
        //     QNAME, closing the DNS-exfiltration channel. Without
        //     these rules, the guest could resolve arbitrary names
        //     even when every TCP connection downstream was blocked.
        out.push(format!(
            "-t nat -A PREROUTING -i tap-engr-+ -p udp --dport 53 \
             -j REDIRECT --to-port {dns_port} \
             -m comment --comment engram-dns-redirect",
        ));
        out.push(format!(
            "-t nat -A PREROUTING -i tap-engr-+ -p tcp --dport 53 \
             -j REDIRECT --to-port {dns_port} \
             -m comment --comment engram-dns-redirect",
        ));
        // 4c. After the REDIRECT, the connection is destined for
        //     the host's local socket; iptables FORWARD doesn't
        //     see it. So we don't need a separate ACCEPT for that
        //     traffic. We DO still want to forbid any *other*
        //     egress: drop everything else from the pool. Notably,
        //     there's no longer an unconditional ACCEPT for
        //     VM→1.1.1.1:53 — every DNS query must come through
        //     the proxy via REDIRECT.
        out.push(format!(
            "-A FORWARD -s {pool} -j DROP \
             -m comment --comment engram-default-deny",
        ));
    } else {
        // 4d. NO-PROXY mode: allow DNS to a public resolver
        //     unconditionally (operator opted out of egress
        //     filtering altogether, and DNS still has to work for
        //     anything in the VM to function). The host-LAN drops
        //     above already filtered private subnets, and
        //     MASQUERADE makes return traffic find the right VM.
        out.push(format!(
            "-A FORWARD -s {pool} -p udp --dport 53 -d {PUBLIC_DNS} \
             -j ACCEPT -m comment --comment engram-dns",
        ));
        out.push(format!(
            "-A FORWARD -s {pool} -p tcp --dport 53 -d {PUBLIC_DNS} \
             -j ACCEPT -m comment --comment engram-dns",
        ));
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

/// Default port the filtering DNS proxy binds on. 5353 not 53 so
/// the host's systemd-resolved (bound on 127.0.0.53:53) can keep
/// running for the host's own name resolution.
pub const DEFAULT_DNS_PORT: u16 = 5353;

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
pub async fn host_startup(proxy_port: Option<u16>, dns_port: Option<u16>) -> Result<(), NetError> {
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
    for line in host_startup_lines(proxy_port, dns_port) {
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
    provision_with_named_tap(vm_cidr, &tap_name).await
}

/// Restore-time variant: the /30 is already reserved (caller passed
/// the manifest's slot through `NetworkAllocator::reserve`) and the
/// TAP name comes from the manifest, not the new sandbox's id. The
/// guest's `state.bin` was snapshotted with the original TAP name on
/// the virtio-net frontend, so we have to recreate it under the same
/// name on the host or FC's snapshot load fails.
#[cfg(target_os = "linux")]
pub async fn provision_with_named_tap(
    vm_cidr: VmCidr,
    tap_name: &str,
) -> Result<NetSetup, NetError> {
    let host_addr = format!("{}/30", vm_cidr.host());

    let _ = run_cmd("ip", &["link", "delete", tap_name]).await;
    run_cmd("ip", &["tuntap", "add", tap_name, "mode", "tap"]).await?;
    run_cmd("ip", &["addr", "add", &host_addr, "dev", tap_name]).await?;
    run_cmd("ip", &["link", "set", "dev", tap_name, "up"]).await?;

    Ok(NetSetup {
        vm_cidr,
        tap_name: tap_name.to_string(),
    })
}

/// Tear down the host-side networking. Best-effort.
#[cfg(target_os = "linux")]
pub async fn teardown(setup: &NetSetup, allocator: &parking_lot::Mutex<NetworkAllocator>) {
    if let Err(e) = run_cmd("ip", &["link", "delete", &setup.tap_name]).await {
        tracing::debug!(tap = %setup.tap_name, error = %e, "tap delete failed");
    }
    allocator.lock().free(setup.vm_cidr);
}

// ============================================================================
// ADR 0014 M1.16 — per-VM network namespaces for warm slots
// ============================================================================
//
// Cold path: each VM owns a TAP in the host root netns with a /30
// from the host pool; the bake's `ip=…` kernel cmdline encodes that
// VM's specific guest IP.
//
// Warm path can't do that — the snapshot's `ip=…` is baked once at
// bake time, so every restore inherits the same guest IP
// (10.200.0.2). FC v1.10.1 forbids changing `host_dev_name` via
// PATCH, so we can't even rebind the TAP per restore. To get N
// concurrent warm slots, each VM runs in its own netns where the
// bake's TAP name + guest IP are reused collision-free. A veth
// pair plugs the netns into the host root; netns-local
// POSTROUTING SNAT rewrites the VM's source IP (10.200.0.2) to a
// unique-per-VM `snat_cidr.guest()` drawn from the same host-pool
// allocator the cold path uses. The egress proxy registry indexes
// against that unique IP, just like for cold sessions.

/// Naming for the netns hosting one warm-restored VM. Resolves
/// under `/var/run/netns/<name>` for both `ip netns exec` and a
/// future direct `setns(2)` path.
pub fn netns_name_for(sandbox_id: SandboxId) -> String {
    let s = sandbox_id.to_string();
    let prefix: String = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(6)
        .collect();
    format!("engr-vm-{prefix}")
}

/// Bind-mount path the kernel publishes per-netns. `ip netns add`
/// creates this; opening + `setns(CLONE_NEWNET)` is how the FC
/// child enters the namespace.
pub fn netns_path_for(sandbox_id: SandboxId) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/var/run/netns/{}", netns_name_for(sandbox_id)))
}

/// veth pair names. Host side `vh-engr-XXXXXX`, netns side
/// `vg-engr-XXXXXX` — both 14 chars, IFNAMSIZ-safe.
pub fn veth_names_for(sandbox_id: SandboxId) -> (String, String) {
    let s = sandbox_id.to_string();
    let prefix: String = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(6)
        .collect();
    (format!("vh-engr-{prefix}"), format!("vg-engr-{prefix}"))
}

/// Per-VM netns bookkeeping. Persisted on `LiveSandbox` so
/// `destroy()` can reverse provisioning. `host_reachable_ip()`
/// is the value the egress proxy registry and `guest_ip()` both
/// expose.
#[derive(Clone, Debug)]
pub struct NetnsSetup {
    /// `engr-vm-<id>` — also resolves to `/var/run/netns/engr-vm-<id>`.
    pub netns_name: String,
    /// Host-root-side veth.
    pub veth_host: String,
    /// Netns-side veth.
    pub veth_ns: String,
    /// TAP name FC opens via `host_dev_name` in state.bin. Created
    /// inside the netns so the bake's name doesn't collide globally.
    pub tap_name: String,
    /// The bake's /30 — `10.200.0.0/30` today, same in every snapshot.
    /// `vm_cidr.guest()` is the kernel-cmdline-assigned VM IP that
    /// every warm restore inherits. The netns's TAP terminates at
    /// `vm_cidr.host()` so the VM's gateway ARP succeeds.
    pub vm_cidr: VmCidr,
    /// Per-VM SNAT slot from the host-pool allocator.
    /// `snat_cidr.guest()` is the SNAT'd source the egress proxy
    /// and dashboard SHELL tab both see.
    pub snat_cidr: VmCidr,
}

impl NetnsSetup {
    /// The host-routable IP identifying this VM from outside the
    /// netns. Egress proxy registry indexes against this; the
    /// shell-tab proxy dials `ws://<this>:7681/ws`.
    pub fn host_reachable_ip(&self) -> Ipv4Addr {
        self.snat_cidr.guest()
    }
}

/// Provision a per-VM netns for a warm restore.
///
/// - Allocates a host-pool /30 for the SNAT source (released by
///   `teardown_netns`).
/// - `ip netns add` + veth pair + bring up `lo`, both veth sides.
/// - Inside the netns: create the bake's TAP, terminate at
///   `bake_cidr.host()` (the VM's expected gateway), default route
///   via the veth-A peer endpoint.
/// - Netns-local `iptables -t nat POSTROUTING SNAT` rewrites
///   source `bake_cidr.guest()` → `snat_cidr.guest()` on egress.
/// - On error, releases the SNAT slot AND best-effort deletes any
///   partially-created netns + veth so a subsequent retry doesn't
///   trip over a leftover.
#[cfg(target_os = "linux")]
pub async fn provision_netns(
    sandbox_id: SandboxId,
    bake_cidr: VmCidr,
    tap_name: &str,
    allocator: &parking_lot::Mutex<NetworkAllocator>,
) -> Result<NetnsSetup, NetError> {
    let snat_cidr = allocator.lock().alloc().map_err(NetError::Alloc)?;
    let res = provision_netns_inner(sandbox_id, bake_cidr, snat_cidr, tap_name).await;
    if let Err(ref e) = res {
        tracing::debug!(error = %e, "provision_netns failed; releasing snat slot");
        let (veth_host, _) = veth_names_for(sandbox_id);
        let netns = netns_name_for(sandbox_id);
        let _ = run_cmd("ip", &["netns", "delete", &netns]).await;
        let _ = run_cmd("ip", &["link", "delete", &veth_host]).await;
        allocator.lock().free(snat_cidr);
    }
    res
}

#[cfg(target_os = "linux")]
async fn provision_netns_inner(
    sandbox_id: SandboxId,
    bake_cidr: VmCidr,
    snat_cidr: VmCidr,
    tap_name: &str,
) -> Result<NetnsSetup, NetError> {
    let netns_name = netns_name_for(sandbox_id);
    let (veth_host, veth_ns) = veth_names_for(sandbox_id);

    // Idempotency: a leftover netns or veth from a prior failed
    // provision would block the additive commands below. Best-
    // effort cleanup first; both `delete` calls are no-ops when
    // nothing's there.
    let _ = run_cmd("ip", &["netns", "delete", &netns_name]).await;
    let _ = run_cmd("ip", &["link", "delete", &veth_host]).await;

    run_cmd("ip", &["netns", "add", &netns_name]).await?;

    // veth pair: A in host root (gets the SNAT-slot host octet),
    // B moved into the netns.
    run_cmd(
        "ip",
        &[
            "link", "add", &veth_host, "type", "veth", "peer", "name", &veth_ns,
        ],
    )
    .await?;
    run_cmd("ip", &["link", "set", &veth_ns, "netns", &netns_name]).await?;
    let veth_host_addr = format!("{}/30", snat_cidr.host());
    run_cmd("ip", &["addr", "add", &veth_host_addr, "dev", &veth_host]).await?;
    run_cmd("ip", &["link", "set", "dev", &veth_host, "up"]).await?;

    // Inside the netns: loopback up, veth-B up + addressed at
    // snat_cidr.guest, TAP created + addressed at bake_cidr.host
    // (the VM's gateway), default route via veth-A peer.
    let veth_ns_addr = format!("{}/30", snat_cidr.guest());
    run_ip_in_netns(&netns_name, &["link", "set", "lo", "up"]).await?;
    run_ip_in_netns(
        &netns_name,
        &["addr", "add", &veth_ns_addr, "dev", &veth_ns],
    )
    .await?;
    run_ip_in_netns(&netns_name, &["link", "set", "dev", &veth_ns, "up"]).await?;
    run_ip_in_netns(&netns_name, &["tuntap", "add", tap_name, "mode", "tap"]).await?;
    let tap_addr = format!("{}/30", bake_cidr.host());
    run_ip_in_netns(&netns_name, &["addr", "add", &tap_addr, "dev", tap_name]).await?;
    run_ip_in_netns(&netns_name, &["link", "set", "dev", tap_name, "up"]).await?;
    let snat_host = snat_cidr.host().to_string();
    run_ip_in_netns(&netns_name, &["route", "add", "default", "via", &snat_host]).await?;

    // Netns-local SNAT. Rewrites every outbound packet's source
    // from `bake_cidr.guest()` (the VM's baked-in eth0 IP) to
    // `snat_cidr.guest()` (this netns's unique pool slot). After
    // SNAT the packet reaches host-root iptables (PREROUTING
    // REDIRECT, MASQUERADE, etc.) carrying the unique source —
    // the egress proxy registry can then map it back to this
    // specific session.
    let snat_guest = snat_cidr.guest().to_string();
    run_iptables_in_netns(
        &netns_name,
        &[
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-o",
            &veth_ns,
            "-j",
            "SNAT",
            "--to-source",
            &snat_guest,
        ],
    )
    .await?;

    Ok(NetnsSetup {
        netns_name,
        veth_host,
        veth_ns,
        tap_name: tap_name.to_string(),
        vm_cidr: bake_cidr,
        snat_cidr,
    })
}

/// Reverse `provision_netns`. Best-effort — log + continue on each
/// step's failure so a partially-broken state doesn't strand the
/// snat slot.
#[cfg(target_os = "linux")]
pub async fn teardown_netns(setup: &NetnsSetup, allocator: &parking_lot::Mutex<NetworkAllocator>) {
    // `ip netns delete` cascades: it removes the TAP, veth-B, and
    // the netns's iptables tables in one shot. veth-A in host root
    // is auto-cleaned by the kernel when its peer disappears.
    if let Err(e) = run_cmd("ip", &["netns", "delete", &setup.netns_name]).await {
        tracing::debug!(netns = %setup.netns_name, error = %e, "netns delete failed");
        // Fall through; veth-A may still need explicit cleanup.
    }
    let _ = run_cmd("ip", &["link", "delete", &setup.veth_host]).await;
    allocator.lock().free(setup.snat_cidr);
}

#[cfg(target_os = "linux")]
async fn run_ip_in_netns(netns: &str, args: &[&str]) -> Result<(), NetError> {
    let mut full = vec!["-n", netns];
    full.extend_from_slice(args);
    run_cmd("ip", &full).await
}

#[cfg(target_os = "linux")]
async fn run_iptables_in_netns(netns: &str, args: &[&str]) -> Result<(), NetError> {
    let mut full = vec!["netns", "exec", netns, "iptables"];
    full.extend_from_slice(args);
    run_cmd("ip", &full).await
}

// Non-Linux stubs.
#[cfg(not(target_os = "linux"))]
pub async fn provision_netns(
    _sandbox_id: SandboxId,
    _bake_cidr: VmCidr,
    _tap_name: &str,
    _allocator: &parking_lot::Mutex<NetworkAllocator>,
) -> Result<NetnsSetup, NetError> {
    Err(NetError::Spawn(
        "provision_netns".into(),
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "FC networking is Linux-only",
        ),
    ))
}

#[cfg(not(target_os = "linux"))]
pub async fn teardown_netns(
    _setup: &NetnsSetup,
    _allocator: &parking_lot::Mutex<NetworkAllocator>,
) {
}

// Non-Linux stubs.
#[cfg(not(target_os = "linux"))]
pub async fn host_startup(
    _proxy_port: Option<u16>,
    _dns_port: Option<u16>,
) -> Result<(), NetError> {
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
pub async fn provision_with_named_tap(
    _vm_cidr: VmCidr,
    _tap_name: &str,
) -> Result<NetSetup, NetError> {
    Err(NetError::Spawn(
        "provision_with_named_tap".into(),
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
        // ADR 0014 M1.16: slot 0 (10.200.0.0/30) is permanently
        // reserved as the bake CIDR, so the first alloc returns
        // slot 1 (10.200.0.4/30). See `NetworkAllocator::new`.
        let mut a = NetworkAllocator::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        let c0 = a.alloc().unwrap();
        let c1 = a.alloc().unwrap();
        let c2 = a.alloc().unwrap();
        assert_eq!(c0.cidr_str(), "10.200.0.4/30");
        assert_eq!(c1.cidr_str(), "10.200.0.8/30");
        assert_eq!(c2.cidr_str(), "10.200.0.12/30");
        // live_count includes the permanently-reserved bake slot
        // plus the 3 just-allocated slots.
        assert_eq!(a.live_count(), 4);
    }

    #[test]
    fn allocator_recycles_freed_slots() {
        let mut a = NetworkAllocator::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        let c0 = a.alloc().unwrap();
        let _c1 = a.alloc().unwrap();
        a.free(c0);
        let reused = a.alloc().unwrap();
        // First non-bake slot is 10.200.0.4/30; freeing then
        // re-allocating returns it.
        assert_eq!(reused.cidr_str(), "10.200.0.4/30");
    }

    #[test]
    fn reserve_holds_specific_slot_then_rejects_dupes() {
        let mut a = NetworkAllocator::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        let target = VmCidr::new(Ipv4Addr::from_str("10.200.0.8").unwrap());
        a.reserve(target).unwrap();
        // Same slot is now taken.
        assert!(matches!(a.reserve(target), Err(AllocError::SlotTaken)));
        // Subsequent alloc returns the lowest available non-reserved
        // slot. Slot 0 is the bake CIDR (perma-reserved); slot 1
        // (10.200.0.4/30) is free; slot 2 (10.200.0.8/30) is our
        // explicit reservation.
        let c0 = a.alloc().unwrap();
        assert_eq!(c0.cidr_str(), "10.200.0.4/30");
        let c2 = a.alloc().unwrap();
        // 10.200.0.8 is reserved; next alloc should skip past it.
        assert_eq!(c2.cidr_str(), "10.200.0.12/30");
    }

    #[test]
    fn reserve_round_trips_through_free() {
        let mut a = NetworkAllocator::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        // The bake CIDR (slot 0) is permanently reserved and not
        // reservable; use slot 1 instead for the recycle round-trip.
        let target = VmCidr::new(Ipv4Addr::from_str("10.200.0.4").unwrap());
        a.reserve(target).unwrap();
        a.free(target);
        // After free, the slot is reservable again.
        a.reserve(target).unwrap();
    }

    /// ADR 0014 M1.16: the bake CIDR (slot 0, `10.200.0.0/30`)
    /// is permanently reserved by [`NetworkAllocator::new`]. Both
    /// `alloc` and `reserve` must refuse to hand it out — otherwise
    /// a netns SNAT slot collides with the bake's eth0 IP and the
    /// SHELL tab fails (observed in prod on session d20c4715).
    #[test]
    fn bake_cidr_slot_0_is_permanently_reserved() {
        let mut a = NetworkAllocator::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        let bake = VmCidr::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        // `reserve(bake)` must fail — slot 0 is already in_use.
        assert!(matches!(a.reserve(bake), Err(AllocError::SlotTaken)));
        // 1000 sequential allocs never produce slot 0.
        for _ in 0..1000 {
            let c = a.alloc().unwrap();
            assert_ne!(
                c.cidr_str(),
                "10.200.0.0/30",
                "alloc must never hand out the bake CIDR"
            );
        }
        // Even after recycling: `free(bake)` is a no-op (slot 0
        // doesn't go onto the free list because it was never
        // alloc()'d in the first place), and a subsequent reserve
        // still fails.
        a.free(bake);
        assert!(matches!(a.reserve(bake), Err(AllocError::SlotTaken)));
    }

    #[test]
    fn reserve_rejects_out_of_pool() {
        let mut a = NetworkAllocator::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
        // Different /16 → not in this allocator's pool.
        let foreign = VmCidr::new(Ipv4Addr::from_str("10.201.0.0").unwrap());
        assert!(matches!(a.reserve(foreign), Err(AllocError::OutOfPool)));
        // Misaligned /30 (lowest two bits set) → not a valid network.
        let misaligned = VmCidr::new(Ipv4Addr::from_str("10.200.0.5").unwrap());
        assert!(matches!(a.reserve(misaligned), Err(AllocError::OutOfPool)));
    }

    #[test]
    fn tap_name_fits_in_ifnamsiz() {
        let n = tap_name_for(id());
        assert!(n.len() <= 15, "tap name {n} too long for IFNAMSIZ");
        assert!(n.starts_with("tap-engr-"));
    }

    #[test]
    fn netns_and_veth_names_are_well_formed() {
        let sid = id();
        let ns = netns_name_for(sid);
        let (vh, vg) = veth_names_for(sid);
        assert!(ns.starts_with("engr-vm-"));
        // netns path is what `ip netns add` publishes under.
        assert_eq!(
            netns_path_for(sid),
            std::path::PathBuf::from(format!("/var/run/netns/{ns}")),
        );
        // Both veth names must fit IFNAMSIZ-1 (15) so the kernel
        // accepts them; verified empirically when veth-pair create
        // calls return EINVAL on too-long names.
        assert!(vh.len() <= 15, "veth host {vh} too long for IFNAMSIZ");
        assert!(vg.len() <= 15, "veth ns {vg} too long for IFNAMSIZ");
        assert!(vh.starts_with("vh-engr-"));
        assert!(vg.starts_with("vg-engr-"));
        // Same sandbox_id must yield the same six-hex-char suffix on
        // all three names so destroy can derive netns/veth names from
        // sandbox_id alone (mirrors `tap_name_for`'s contract).
        let tap = tap_name_for(sid);
        let tap_suffix = &tap["tap-engr-".len()..];
        let vh_suffix = &vh["vh-engr-".len()..];
        let vg_suffix = &vg["vg-engr-".len()..];
        let ns_suffix = &ns["engr-vm-".len()..];
        assert_eq!(tap_suffix, vh_suffix);
        assert_eq!(tap_suffix, vg_suffix);
        assert_eq!(tap_suffix, ns_suffix);
    }

    #[test]
    fn netns_setup_host_reachable_ip_is_snat_guest() {
        let setup = NetnsSetup {
            netns_name: "engr-vm-abc123".into(),
            veth_host: "vh-engr-abc123".into(),
            veth_ns: "vg-engr-abc123".into(),
            tap_name: "tap-engr-abc123".into(),
            vm_cidr: VmCidr::new(Ipv4Addr::from_str("10.200.0.0").unwrap()),
            snat_cidr: VmCidr::new(Ipv4Addr::from_str("10.200.0.4").unwrap()),
        };
        // Egress proxy registry indexes here; the dashboard SHELL
        // tab dials `ws://<this>:7681/ws`. Every warm VM has the
        // same `vm_cidr.guest()` (10.200.0.2) inside, but a
        // unique `snat_cidr.guest()` on the host-visible side.
        assert_eq!(
            setup.host_reachable_ip(),
            Ipv4Addr::from_str("10.200.0.6").unwrap()
        );
    }

    #[test]
    fn host_startup_no_proxy_drops_lan_but_allows_internet() {
        let lines = host_startup_lines(None, None).join("\n");
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
        let lines = host_startup_lines(Some(9443), None).join("\n");
        assert!(lines.contains("-i tap-engr-+ -p tcp --dport 443"));
        assert!(lines.contains("--to-port 9443"));
        assert!(lines.contains("engram-default-deny"));
        assert!(lines.contains("--dport 9443 -j ACCEPT"));
        assert!(lines.contains("engram-proxy-input"));
    }

    #[test]
    fn host_startup_with_proxy_redirects_dns_through_filtering_proxy() {
        // The DNS-exfil close: no unconditional ACCEPT to a public
        // resolver; instead, REDIRECT every guest DNS query to the
        // local filtering proxy regardless of which upstream IP
        // they chose.
        let lines = host_startup_lines(Some(9443), Some(5353));
        let joined = lines.join("\n");
        // No more blanket ACCEPT for VM→1.1.1.1:53 in proxy mode.
        assert!(
            !joined.contains("--dport 53 -d 1.1.1.1"),
            "proxy mode must not have an unconditional ACCEPT to 1.1.1.1:53; \
             that's exactly the DNS-exfil channel this change closes",
        );
        // REDIRECT for both transports, targeting the proxy's DNS port.
        assert!(joined.contains("-i tap-engr-+ -p udp --dport 53"));
        assert!(joined.contains("-i tap-engr-+ -p tcp --dport 53"));
        assert!(joined.contains("--to-port 5353"));
        // INPUT accept for the proxy's DNS port (so REDIRECTed
        // packets aren't caught by the blanket VM-INPUT drop).
        assert!(joined.contains("-p udp --dport 5353 -j ACCEPT"));
        assert!(joined.contains("-p tcp --dport 5353 -j ACCEPT"));
        // INPUT accepts (incl. DNS) must sit before the
        // engram-host-input DROP.
        let drop_idx = lines
            .iter()
            .position(|l| {
                l.contains("comment engram-host-input ") || l.ends_with("comment engram-host-input")
            })
            .expect("host-input drop present");
        let dns_input_idx = lines
            .iter()
            .position(|l| l.contains("engram-proxy-dns-input"))
            .expect("dns input accept present");
        assert!(
            dns_input_idx < drop_idx,
            "DNS input ACCEPT must precede the engram-host-input DROP",
        );
    }

    #[test]
    fn host_startup_dns_port_defaults_to_5353_when_none() {
        // None dns_port falls through to DEFAULT_DNS_PORT so the
        // operator only needs to override when something else on the
        // host already binds 5353.
        let lines = host_startup_lines(Some(9443), None).join("\n");
        assert!(lines.contains("--to-port 5353"));
        assert!(lines.contains("--dport 5353 -j ACCEPT"));
    }

    #[test]
    fn host_startup_no_proxy_keeps_dns_to_public_resolver() {
        // The operator opted out of filtering altogether; we keep
        // the legacy "DNS allowed to 1.1.1.1" path so the VM can
        // resolve at all. The DNS-exfil hole is exactly the price
        // of `--egress-proxy-port=0`.
        let lines = host_startup_lines(None, None).join("\n");
        assert!(lines.contains("--dport 53 -d 1.1.1.1"));
        assert!(!lines.contains("engram-dns-redirect"));
    }

    #[test]
    fn host_startup_proxy_input_accept_comes_before_drop() {
        // Ordering matters — ACCEPT before DROP — otherwise the
        // blanket VM-INPUT drop catches the REDIRECTed proxy
        // traffic too. The two sit consecutively in the output;
        // assert ACCEPT line index < DROP line index.
        let lines = host_startup_lines(Some(9443), None);
        let accept_idx = lines.iter().position(|l| l.contains("engram-proxy-input"));
        let drop_idx = lines.iter().position(|l| {
            l.contains("comment engram-host-input ") || l.ends_with("comment engram-host-input")
        });
        assert!(accept_idx.is_some() && drop_idx.is_some());
        assert!(accept_idx < drop_idx);
    }

    /// ADR 0014 issue #6 follow-up: regression guard. The host-agent
    /// ProxyShell dials the in-guest ttyd from host root (cold path)
    /// and the guest's SYN+ACK return packet hits the INPUT chain.
    /// Without an ESTABLISHED,RELATED ACCEPT *before* the blanket
    /// engram-host-input DROP, every host→VM TCP dial times out
    /// (observed in prod against session 379abfec on 2026-05-20).
    /// This test pins the rule's presence and ordering.
    #[test]
    fn host_startup_accepts_established_input_before_drop() {
        let lines = host_startup_lines(Some(9443), Some(5353));
        let est_idx = lines
            .iter()
            .position(|l| l.contains("engram-host-input-established"))
            .expect("host-input ESTABLISHED ACCEPT must be present");
        let drop_idx = lines
            .iter()
            .position(|l| {
                l.contains("comment engram-host-input ") || l.ends_with("comment engram-host-input")
            })
            .filter(|i| *i != est_idx)
            .expect("host-input DROP must be present and distinct from the ESTABLISHED rule");
        assert!(
            est_idx < drop_idx,
            "ESTABLISHED,RELATED ACCEPT must precede the engram-host-input DROP \
             so host-initiated TCP flows (proxy_shell ↔ ttyd) see their SYN+ACK \
             return packets",
        );
        let est_line = &lines[est_idx];
        assert!(
            est_line.contains("--ctstate ESTABLISHED,RELATED"),
            "rule must match by conntrack state, not by port/protocol — \
             keeps the VM→host attack surface narrow"
        );
        // Also assert the rule is present in no-proxy mode (which
        // operators use when egress filtering is disabled).
        let nolines = host_startup_lines(None, None);
        assert!(
            nolines
                .iter()
                .any(|l| l.contains("engram-host-input-established")),
            "ESTABLISHED ACCEPT must be present in both proxy and no-proxy modes"
        );
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
