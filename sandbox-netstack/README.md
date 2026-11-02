# Private sandbox network helper

This artifact provides one IPv4 netstack per VM. It has no host IP listener,
route mutation, external resolver, destination dial, switch, or NAT. It does not
activate a backend or supply a final managed-policy proof. Creation remains
blocked until the backend and durable boot producer supply their own checks.

Build with Go **1.26.2**, `GOTOOLCHAIN=local`. The exact module closure is in
`go.mod` and `go.sum`. The gVisor pin is
`v0.0.0-20260413194555-9680d69bf798`; its module hash is
`h1:7h21uRYtzPJjXp9W/BZLx68z8TwW8+h5BPdI4P5ZkJ0=` and its module-file hash is
`h1:xQ2PWgHmWJA/Ph4i1q1jBm39BKhc3W0DXqWoDSyuBOY=`. DNS decoding uses
`golang.org/x/net/dns/dnsmessage` v0.44.0. No floating dependency is allowed.

From the repository root:

```sh
sandbox-netstack/build.sh test
sandbox-netstack/build.sh race
sandbox-netstack/build.sh build
```

The optional second argument selects an owned output directory. The default is
`var/mcp-netstack-build`. The script rejects cache symlinks and uses that directory
for every Go cache and temporary file. The build writes
`engram-sandbox-netstack` there, with `CGO_ENABLED=0` and `-trimpath`. The race gate
uses `CGO_ENABLED=1` and needs a native C compiler. The tests build and execute the
real helper, so the Go tool must be available when tests run. Packaging and CI
must run these commands explicitly; Cargo does not run this module.

## Descriptor and startup contract

Start the executable with no arguments. Supply only these three descriptors;
use the process API's stdin/stdout/stderr descriptor mapping. These descriptors
are channels, **not logs**. Clear the child environment. All other descriptors
must be close-on-exec. Retain the parent control peer and child process handle.

| Descriptor | Type | Owner and use |
|---|---|---|
| 0 | Connected AF_UNIX datagram | One VM Ethernet attachment; one datagram per frame |
| 1 | Connected AF_UNIX stream | Parent lifetime and explicit OPEN/CLOSE control |
| 2 | Connected AF_UNIX stream | Registered private proxy allocation capability |

The helper duplicates each inherited descriptor into Go's poller and closes the
original. The parent must close its child-side copies immediately after spawn.
The parent must terminate and reap the helper on every failed startup, socket
loss, VM teardown, or invalidated generation. Do not infer lifetime from datagram
EOF. Do not pass the capability through argv, environment, disk, guest data, or
operational logs. Do not retain a second copy across host process generations.

Register descriptor 2 first through `ProxydHandle::register_private_network`, or
`engram_egress_proto::private::register` on a dedicated host UDS connection. The
host-only upgrade is `ToProxyd::PrivateNetwork`, followed by the bounded
`private::Allocation`. It binds host ID/incarnation, proxy incarnation,
session/sandbox, boot attempt, attachment generation, exact guest/gateway IPv4,
and the current managed policy revision/digest. Registration requires a live
managed policy, starts CLOSED, and has no placement epoch. The returned socket
has consumed the CLOSED registration reply; transfer that socket to the helper.

The host sends this configuration on descriptor 1 within five seconds:

```json
{"version":1,"guest":"192.0.2.2","gateway":"192.0.2.1","mac":"02:00:00:00:00:02"}
```

Addresses must match the registered allocation and actual attachment. The host
must record and check that exact shape in its backend proof. The gateway MAC is
`02:00:00:00:00:01`. The guest MAC must be a different unicast MAC. MTU is exactly
1500. Only Ethernet IPv4 and gateway ARP are accepted. The helper returns:

```json
{"version":1,"sequence":0,"epoch":0,"state":"closed"}
```

After obtaining the actual placement epoch, the parent sends:

```json
{"version":1,"kind":"open","epoch":7}
```

The helper binds that epoch once on the proxy channel, drains closed-phase
packets, then opens packet admission. The response is sequence 1, the same epoch,
and state `ready`. This is **transport readiness only**. It cannot satisfy the
backend network proof, durable boot release, or final tool-exposure ACK.

CLOSE uses `{"version":1,"kind":"close","epoch":7}`. A successful response has
sequence 2 and state `closed`. CLOSE is terminal. OPEN replay, wrong epoch,
malformed control, or either reliable channel's loss invalidates the allocation.
There is no auto-reopen or reconnect. Use a new allocation after loss.

## Private flow contract

Frames are a 4-byte big-endian length followed by strict JSON, at most 2048 bytes.
The first byte may wait while idle. A partial frame and each response write have
a five-second budget. Every request has version 1, a strictly increasing sequence
starting at 1, and the exact bound epoch. Operations are nested:

```json
{"version":1,"sequence":1,"epoch":7,"operation":{"kind":"bind"}}
{"version":1,"sequence":2,"epoch":7,"operation":{"kind":"tcp","destination":"198.51.100.7","port":8443}}
```

`bind` returns `ready`. `tcp` and `dns` require one connected AF_UNIX stream via
SCM_RIGHTS on a single byte `0xED`, immediately after the header. The receiver
sets close-on-exec and rejects extra descriptors, wrong socket types, unknown
fields, missing ports, sequence replay, wrong epochs, and oversized input.
`accepted` on the allocation channel means the flow is owned by the session
lease; it is not an allow decision. The handed-off TCP stream contains guest TLS
bytes and goes through the existing Rust policy dispatcher on every port.

DNS uses `kind: "dns"`, the exact allocation gateway, and port 53. Its stream
contains one two-byte-length-prefixed DNS request and one matching response.
The existing Rust DNS name policy and resolver own the decision. The helper
has no external DNS socket. TCP 53 on any other address goes to the ordinary TLS
dispatcher and plaintext DNS fails. UDP to any other port/address is dropped.

## Bounds and cancellation

Each helper has 64 active flows, at most 8 DNS exchanges, 16 pending TCP
handshakes, a 64-packet link queue, and 64 KiB TCP send/receive buffers. The pinned
gVisor IPv4 reassembly cache trims at 4 MiB to 3 MiB. DNS bodies are limited to
4096 bytes; UDP DNS must also fit the MTU and cannot be fragmented. TCP fragments
are reassembled by gVisor. Other UDP, ICMP input, IPv6, VLAN, spoofed MAC/IPv4/ARP,
short frames, oversized frames, and `MSG_TRUNC` are rejected.

DNS exchanges have a ten-second budget. TCP flow lifetime is capped at five
minutes. Packet output waits at most 250 ms. Gateway response writes wait at
most one second. OPEN drains at most 512 queued datagrams; an input flood fails
activation. A closed packet is never stored for later replay.

Allocation identity comes only from the inherited capability. A guest IP, MAC,
header, or payload cannot select another session. The proxy holds the existing
stable session lease before reading policy or TLS. Replacement and revocation
cancel and join owned Rust TCP/DNS/HTTP2 work. Allocation tombstones are bounded
at 4096 per proxy incarnation; exhaustion fails closed. Attachment generations
must increase within a host incarnation. Retired host incarnations cannot return.
An unrelated session has a different admission/drain lock.

Revocation uses the reliable private stream, never datagram EOF. The proxy
first cancels and joins its TCP/DNS work, closes its write half, and waits up to
five seconds for helper EOF. On that stop signal the helper closes its gate,
closes the packet descriptor, and joins all packet readers/writers, stack work,
flow/DNS work, and control readers before it closes the private descriptor.
The stable session lease remains held through this barrier. Pending descriptor
carriers are drained with an OS-maximum ancillary buffer and closed.

If cleanup EOF is absent, malformed, or fails, the proxy poisons that session's
stable gate. Apply, full replacement, and removal cannot acknowledge success;
a fresh allocation or ordinary policy replay cannot reopen it. Other sessions
remain usable. Recovery requires a fresh proxy incarnation plus the existing
full policy replay and quarantine checks. The host adapter must treat any
failed cleanup as a failed enforcement receipt and replace the proxy before
recovery. A helper OPEN or CLOSE response is never the final enforcement ACK.
Bytes already delivered before the cleanup barrier cannot be revoked.
