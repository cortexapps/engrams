# Runbook: `just dev-fc` — the Firecracker backend on a Mac via Colima

ADR 0082. Runs the real Firecracker backend in the local dev stack on Apple
Silicon: the host-agent (+ FC, NBD, UFFD, netns egress, squashfs bundles) runs
inside a dedicated Colima VM with a nested-virt `/dev/kvm`, while the
coordinator, orchestrator, web, and docker-compose deps stay on the Mac
exactly as in plain `just dev`. This is the only way to exercise the FC-only
code paths in the Mac inner loop; VZ (`just dev`) remains the zero-setup
default.

**The rig is an aarch64 analog of production** (nested virt is
same-architecture only): same kernel recipe, same FC release train, same code
paths — not the prod x86_64 binaries. x86_64 CI (`test-firecracker`) and the
GCP dev-vm stay authoritative. For the manual guest-poking variant of this rig
(hand-booted FC guests, kernel experiments), see
`docs/runbooks/local-firecracker-colima.md`.

Requirements: M3 or newer (nested virt is hardware-gated), macOS 15+, Colima
≥ 0.8, `jq`, nix (the cross-compile and macOS `mksquashfs` both come from the
flake devshell).

## First-time setup

```sh
just fc-colima-provision            # default profile fc-dev; idempotent
```

This creates/starts the profile (`--vm-type vz --nested-virtualization`,
6 CPU / 16 GiB / 60 GiB), and inside it: installs packages, exposes
`/opt/engram-dev/bin/mke2fs` for enable-time materialization, installs the
upstream `firecracker` aarch64 binary (CI's pinned version), loads `nbd`
(`nbds_max=16`, persisted — without it base-snapshot capture at image-enable
breaks), sets `vm.unprivileged_userfaultfd=1`, installs the loopback
forwarders (below), and builds the aarch64 guest kernel natively in the VM
(~4 min, one-time; `--rebuild-kernel` to force). It captures and restores your
docker CLI context around `colima start` — your default Colima docker daemon
is untouched.

## Daily use

```sh
just dev-fc                         # = ENGRAM_FC_COLIMA_PROFILE=fc-dev tilt up
just bake-demo && just integration-session   # same flow as any other backend
```

`just dev-fc` is sugar for setting `ENGRAM_FC_COLIMA_PROFILE`; persist that in
`.env` if you want plain `just dev` to mean this mode. With the var set the
Tiltfile skips the backend probe (the VM's `/dev/kvm` is invisible from the
Mac), forces `firecracker`, and fails fast at parse time if the VM isn't
provisioned/running. Unset, behavior is byte-for-byte stock.

Tilt's host-agent resource then: cross-compiles `engram-host-agent` +
`engram-uffd-handler` to `aarch64-unknown-linux-musl` (static, via
`nix develop`), tar-pipes the binaries and the staged bundle dir
(`var/shared`) onto the VM's own disk (FC serves bundle files as block-device
backings — never off the virtiofs mount), and launches the agent in the VM
under `sudo` with the same env contract as the Linux dev path. The launch env
sets `ENGRAM_MKE2FS=/opt/engram-dev/bin/mke2fs`; rerun
`just fc-colima-provision` to repair an older profile that predates this path.

## How the networking works (and why nothing had to move)

- **Mac → VM** (coordinator → host-agent gRPC — every RPC, shell keystroke,
  and preview byte): the agent binds `0.0.0.0:9101` in the VM; Lima
  auto-forwards guest listeners to the Mac's `127.0.0.1`, so it still
  advertises `http://127.0.0.1:9101` and the coordinator can't tell it moved.
- **VM → Mac** (register/heartbeat, OCI pulls, GCS chunks): Lima guests reach
  Mac-loopback services via the host gateway `192.168.5.2`. The coordinator
  endpoint is simply `http://192.168.5.2:8090`. The registry, fake-gcs, and
  jaeger instead get an OUTPUT DNAT in the VM (`engram-dev-fwd.service`:
  localhost:{5001,4443,4317} → gateway, via `route_localnet` + MASQUERADE) so
  that `localhost:5001/...` image refs and `engram-oci`'s loopback-only
  plaintext HTTP allowance keep working unmodified. Deliberately NOT loopback
  listeners (the original socat design): Lima auto-forwards guest loopback
  listeners onto the Mac, where they'd shadow the real deps.

## Gotchas

- **`bundles` is manual-trigger on this path.** The `bundles` resource builds
  once at `tilt up` (including the built-in `claude` harness — cross-compiled +
  the pinned `claude` CLI, ADR 0062) and then does **not** auto-rebuild: the
  Docker-built bundles under colima's file-sharing bump the ctime of the whole
  `deploy/bundles` tree, which would otherwise retrigger Tilt's deps watcher in
  a loop. After an intentional skill edit, re-trigger `bundles` from the Tilt
  UI (VZ `just dev` keeps auto-rebuild). If a session 400s with *"built-in
  harness `claude` … is not staged on any host yet"*, the bundles build was
  skipped (e.g. no Docker / not in `nix develop`) — re-trigger it and confirm
  `var/shared/current.json` carries a `harness-claude` key.
- **Session create 400s with "no host could restore … tcp connect error" /
  the coordinator can't reach the host.** The coordinator dials the host-agent
  at its advertised `127.0.0.1:9101`, reachable only via a guest→Mac forward.
  Lima's built-in auto-forward is edge-triggered and unreliable across
  host-agent/VM restarts (it silently stops forwarding 9101), so the
  `fc-grpc-forward` Tilt resource holds it deterministically with `ssh -L`
  instead. If dial-in fails, check that resource is green and 9101 is open
  (`nc -z 127.0.0.1 9101`); re-trigger `fc-grpc-forward` in the Tilt UI if not
  (e.g. after a VM restart changed its ssh port). The host being `ready`
  doesn't imply reachable — register/heartbeat run the other direction.
- **Session stuck `queued`, coordinator logs "no capacity".** The host-agent
  keeps each enabled image's base-snapshot memory image resident (~the session
  mem budget, 4 GiB by default), so on a too-small VM there's no room for the
  cache + a session. The VM is provisioned at 16 GiB for this reason; if you
  shrank it, sessions queue forever. Check `allocatable_mib` in the `hosts`
  table vs the session's `mem_budget_mib`.
- **Claude sessions need egress + a per-session policy.** The egress proxy is
  mandatory (issue #240) — it's the only path a guest reaches the network — and
  `dev-fc` runs it on `8443`. Port `0` is rejected because it would create a
  dead `:443→0` redirect while the proxy binds a random ephemeral port.
  For a claude session to actually reach the API you also set, per session in
  the UI: (1) `api.anthropic.com` (and `statsig.anthropic.com`) in the policy's
  `network.allow_hosts` — ADR 0057, it comes from the session policy, NOT the
  image manifest, and `network.default="allow"` does not substitute; (2)
  `ANTHROPIC_API_KEY` (Literal secret → guest env; resolved by the coord's
  `EnvSecretStore` from its own `$ANTHROPIC_API_KEY`, so export it before
  `just dev-fc`, or supply it directly as `harness_env`). The anthropic call
  takes the proxy's Bypass/SNI-splice path, so no guest CA-trust setup is
  needed. A `--dev-vm` session needs none of this.
- **`:443` redirects to a stale port after many host-agent restarts.**
  `host_startup`'s iptables install is additive-idempotent (a `-C` check per
  exact rule), so if the proxy port ever changed across restarts the old
  `engram-proxy-redirect … redir ports <old>` PREROUTING rules linger BEFORE
  the new one and win (first-match). Symptom: proxy listening on the right port
  but guest HTTPS still dead. Fix: delete the stale rules —
  `colima ssh --profile fc-dev -- sudo iptables -t nat -L PREROUTING -n --line-numbers | grep dpt:443`
  then `sudo iptables -t nat -D PREROUTING <n>` the wrong-port ones. A fresh
  `dev-fc` (single start) doesn't accumulate them.
- **`tilt down` does not stop the remote host-agent** (or its live microVMs).
  `colima ssh` doesn't propagate signals, so each (re)start pre-kills the
  prior instance instead; between `tilt down` and the next `dev-fc` the old
  agent lingers. Manual stop:
  `colima ssh --profile fc-dev -- sudo pkill -f engram-host-agent`.
- **Any `colima start` steals the docker context.** The provision script
  restores it; if you start the profile by hand, run
  `docker context use colima` (or whatever your daemon profile is) after.
- **Base snapshots are per-backend.** Switching between `dev` (VZ) and
  `dev-fc` re-captures the base snapshot per enabled image on first enable —
  expected, not a bug.
- **Two-host mode** (`ENGRAM_INTEG_TWO_HOSTS`) is rejected in this mode —
  wiring a second agent out of the same VM isn't built yet.
- The VM keeps running after `tilt down` (`colima stop fc-dev` if you want it
  gone; it's cheap to leave up).

## Troubleshooting

- *Parse-time failure "no guest kernel at /opt/engram-dev/Image" or
  "no mke2fs at /opt/engram-dev/bin/mke2fs"* — run
  `just fc-colima-provision` (VM missing, stopped, never provisioned, or
  provisioned before the mke2fs contract path existed).
- *Image pull fails in the host-agent* — check the DNAT forwarder:
  `colima ssh --profile fc-dev -- systemctl status engram-dev-fwd` (and its
  rules: `colima ssh --profile fc-dev -- sudo iptables -t nat -S OUTPUT`),
  and that the Mac-side registry answers: `curl http://localhost:5001/v2/`.
- *Base-snapshot capture fails at image-enable* — check NBD in the VM:
  `colima ssh --profile fc-dev -- ls /dev/nbd0` (provision persists the
  module; a re-run fixes it).
- *Register/heartbeat failing* — the coordinator must be up on the Mac
  (`:8090`); from the VM, `curl http://192.168.5.2:8090/healthz` should
  answer.
