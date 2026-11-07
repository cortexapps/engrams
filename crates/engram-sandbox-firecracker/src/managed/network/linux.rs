use super::*;
use serde_json::Value;
use std::{
    fs::File,
    os::{
        fd::AsRawFd,
        unix::{fs::MetadataExt, process::CommandExt},
    },
    process::{Command, Stdio},
    sync::Arc,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LiveIdentity {
    pub namespace_device: u64,
    pub namespace_inode: u64,
    pub tap_ifindex: u32,
    pub rules_sha256: String,
}

pub(in crate::managed) struct Namespace {
    pub fd: File,
    pub shape: NetworkShape,
    pub identity: LiveIdentity,
    links: Value,
    addresses: Value,
    routes: Value,
    rules: Value,
    released: std::sync::atomic::AtomicBool,
}
impl Namespace {
    pub async fn create(
        shape: NetworkShape,
    ) -> io::Result<(Arc<Self>, std::net::TcpListener, std::net::UdpSocket)> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        // Never change a runtime worker's namespace. This thread cannot return
        // to a pool, and only inherited child processes enter the new namespace.
        std::thread::Builder::new()
            .name("managed-net-create".into())
            .spawn(move || {
                let _ = tx.send(Self::create_inner(shape));
            })?;
        rx.await
            .map_err(|_| io::Error::other("network creation worker stopped"))?
    }
    fn create_inner(
        shape: NetworkShape,
    ) -> io::Result<(Arc<Self>, std::net::TcpListener, std::net::UdpSocket)> {
        // SAFETY: this dedicated native thread has no shared network activity.
        if unsafe { libc::unshare(libc::CLONE_NEWNET) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = File::open("/proc/thread-self/ns/net")?;
        let metadata = fd.metadata()?;
        let mut owner = Self {
            fd,
            shape,
            identity: LiveIdentity {
                namespace_device: metadata.dev(),
                namespace_inode: metadata.ino(),
                tap_ifindex: 0,
                rules_sha256: String::new(),
            },
            links: Value::Null,
            addresses: Value::Null,
            routes: Value::Null,
            rules: Value::Null,
            released: std::sync::atomic::AtomicBool::new(false),
        };
        owner.run(
            "sysctl",
            &[
                "-q",
                "-w",
                "net.ipv6.conf.all.disable_ipv6=1",
                "net.ipv6.conf.default.disable_ipv6=1",
            ],
            None,
        )?;
        owner.run("ip", &["tuntap", "add", "dev", TAP, "mode", "tap"], None)?;
        owner.run(
            "ip",
            &[
                "link",
                "set",
                "dev",
                TAP,
                "address",
                GATEWAY_MAC,
                "mtu",
                "1500",
            ],
            None,
        )?;
        // No link is up and no FC process exists at this point.
        owner.run("nft", &["-f", "-"], Some(rules(&owner.shape).as_bytes()))?;
        owner.rules = owner.json("nft", &["-j", "-s", "list", "ruleset"])?;
        verify_closed_tables(&owner.rules)?;
        owner.run(
            "ip",
            &[
                "address",
                "add",
                &format!("{}/24", owner.shape.gateway),
                "dev",
                TAP,
            ],
            None,
        )?;
        let before_up = owner.link_shape()?;
        let tap_ifindex = verify_links(&before_up, &owner.shape)?;
        if before_up
            .as_array()
            .is_some_and(|links| links.iter().any(|v| v["up"] == true))
        {
            return Err(io::Error::other("namespace link was already up"));
        }
        owner.run("ip", &["link", "set", "dev", TAP, "up"], None)?;
        owner.links = owner.link_shape()?;
        owner.addresses = owner.json("ip", &["-j", "address", "show"])?;
        normalize_addresses(&mut owner.addresses);
        owner.routes = owner.route_shape()?;
        owner.identity.tap_ifindex = verify_links(&owner.links, &owner.shape)?;
        if owner.identity.tap_ifindex != tap_ifindex {
            return Err(io::Error::other("TAP changed during setup"));
        }
        if !owner
            .links
            .as_array()
            .is_some_and(|links| links.iter().any(|v| v["ifname"] == TAP && v["up"] == true))
        {
            return Err(io::Error::other("TAP is not up"));
        }
        use sha2::{Digest, Sha256};
        owner.identity.rules_sha256 =
            hex::encode(Sha256::digest(serde_json::to_vec(&owner.rules)?));
        // Binding happens in this native namespace thread. No port is public.
        let tcp = std::net::TcpListener::bind((owner.shape.gateway, PROXY_PORT))?;
        let dns = std::net::UdpSocket::bind((owner.shape.gateway, 53))?;
        tcp.set_nonblocking(true)?;
        dns.set_nonblocking(true)?;
        owner.check(None)?;
        Ok((Arc::new(owner), tcp, dns))
    }
    pub fn command(&self, program: &std::ffi::OsStr) -> io::Result<Command> {
        let mut cmd = Command::new(program);
        let fd = self.fd.as_raw_fd();
        // SAFETY: the owner retains fd through synchronous spawn. setns is the
        // only child-side change; no name or numeric PID selects this namespace.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setns(fd, libc::CLONE_NEWNET) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(cmd)
    }
    fn run(&self, program: &str, args: &[&str], input: Option<&[u8]>) -> io::Result<Vec<u8>> {
        use std::io::{Read, Write};
        let mut child = self
            .command(program.as_ref())?
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let result = (|| {
            if let Some(bytes) = input {
                child
                    .stdin
                    .take()
                    .ok_or_else(|| io::Error::other("missing command input"))?
                    .write_all(bytes)?;
            }
            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("missing command output"))?;
            // SAFETY: the owned pipe remains open. Preserve all existing flags.
            let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
            if flags < 0
                || unsafe {
                    libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK)
                } < 0
            {
                return Err(io::Error::last_os_error());
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut bytes = Vec::new();
            let mut buffer = [0; 8192];
            loop {
                match stdout.read(&mut buffer) {
                    Ok(n) => {
                        bytes.extend_from_slice(&buffer[..n]);
                        if n == 0 {
                            if let Some(status) = child.try_wait()? {
                                return if status.success() {
                                    Ok(bytes)
                                } else {
                                    Err(io::Error::other("managed network command failed"))
                                };
                            }
                        }
                    }
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
                if bytes.len() >= 1024 * 1024 || std::time::Instant::now() >= deadline {
                    return Err(io::Error::other(
                        "managed network command exceeded its limit",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })();
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }
    fn json(&self, command: &str, args: &[&str]) -> io::Result<Value> {
        serde_json::from_slice(&self.run(command, args, None)?).map_err(io::Error::other)
    }
    fn link_shape(&self) -> io::Result<Value> {
        stable_link_shape(self.json("ip", &["-j", "-d", "link", "show"])?)
    }
    fn route_shape(&self) -> io::Result<Value> {
        let mut routes = self.json("ip", &["-j", "route", "show", "table", "all"])?;
        if let Some(items) = routes.as_array_mut() {
            for item in items {
                if let Some(flags) = item["flags"].as_array_mut() {
                    flags.retain(|v| v != "linkdown");
                }
            }
        }
        Ok(routes)
    }
    /// Remove only the two retained CLOSED tables after the adapter's exact ACK.
    pub fn release(&self, child_pid: u32) -> io::Result<()> {
        self.check(Some(child_pid))?;
        if self.released.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(());
        }
        self.run(
            "nft",
            &["-f", "-"],
            Some(b"delete table netdev closed\ndelete table inet closed\n"),
        )?;
        if self.json("nft", &["-j", "-s", "list", "ruleset"])? != self.released_rules() {
            return Err(io::Error::other("released network rules changed"));
        }
        self.released
            .store(true, std::sync::atomic::Ordering::Release);
        self.check(Some(child_pid))
    }
    fn released_rules(&self) -> Value {
        let mut rules = self.rules.clone();
        if let Some(items) = rules["nftables"].as_array_mut() {
            items.retain(|item| {
                item["table"]["name"] != "closed"
                    && item["chain"]["table"] != "closed"
                    && item["rule"]["table"] != "closed"
            });
        }
        rules
    }
    pub fn check(&self, child_pid: Option<u32>) -> io::Result<()> {
        let metadata = self.fd.metadata()?;
        if metadata.dev() != self.identity.namespace_device
            || metadata.ino() != self.identity.namespace_inode
        {
            return Err(io::Error::other("namespace changed"));
        }
        if let Some(pid) = child_pid {
            let child = std::fs::metadata(format!("/proc/{pid}/ns/net"))?;
            if child.dev() != metadata.dev() || child.ino() != metadata.ino() {
                return Err(io::Error::other("VM namespace changed"));
            }
        }
        let mut addresses = self.json("ip", &["-j", "address", "show"])?;
        normalize_addresses(&mut addresses);
        if self.link_shape()? != self.links
            || addresses != self.addresses
            || self.route_shape()? != self.routes
            || self.json("nft", &["-j", "-s", "list", "ruleset"])?
                != if self.released.load(std::sync::atomic::Ordering::Acquire) {
                    self.released_rules()
                } else {
                    self.rules.clone()
                }
        {
            return Err(io::Error::other("managed network shape changed"));
        }
        Ok(())
    }
}
fn normalize_addresses(value: &mut Value) {
    if let Some(items) = value.as_array_mut() {
        for item in items {
            if let Some(object) = item.as_object_mut() {
                for key in ["flags", "operstate", "linkmode", "qdisc"] {
                    object.remove(key);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Task 8 must invoke this explicitly on an isolated Linux root worker.
    /// This checks installed state; VM and packet acceptance are separate tests.
    #[tokio::test]
    #[ignore = "requires isolated Linux root, iproute2, nftables and netdev egress support"]
    async fn actual_closed_namespace_readback_detects_route_tampering() {
        // SAFETY: read-only process identity query.
        assert_eq!(
            unsafe { libc::geteuid() },
            0,
            "this explicit acceptance check needs root"
        );
        let root_before = File::open("/proc/thread-self/ns/net")
            .unwrap()
            .metadata()
            .unwrap();
        let shape =
            NetworkShape::new("192.0.2.2".parse().unwrap(), "192.0.2.1".parse().unwrap()).unwrap();
        let (namespace, tcp, dns) = Namespace::create(shape).await.unwrap();
        assert_eq!(tcp.local_addr().unwrap().port(), PROXY_PORT);
        assert_eq!(dns.local_addr().unwrap().port(), 53);
        let checked = namespace.clone();
        tokio::task::spawn_blocking(move || {
            checked.check(None).unwrap();
            checked
                .run("ip", &["route", "add", "blackhole", "default"], None)
                .unwrap();
            assert!(checked.check(None).is_err());
        })
        .await
        .unwrap();
        drop(tcp);
        drop(dns);
        drop(namespace);
        let root_after = File::open("/proc/thread-self/ns/net")
            .unwrap()
            .metadata()
            .unwrap();
        assert_eq!(
            (root_before.dev(), root_before.ino()),
            (root_after.dev(), root_after.ino())
        );
    }
}
