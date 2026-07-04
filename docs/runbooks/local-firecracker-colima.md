# Runbook: real Firecracker on a Mac — Colima nested-virt validation rig

How to boot real Firecracker microVMs locally on Apple Silicon and use them to
validate guest-kernel and in-guest behavior (built for issue #569 / ADR 0067:
the chromium userns-sandbox verdict, the browser-bundle glibc fix, and the
launcher watchdog teardown test). This is the *local* complement to the two
canonical Linux+KVM environments — CI's `test-firecracker` lane (`ci.yml`) and
the `engram-dev` GCP box (`docs/dev-vm.md`) — which remain the authority.

**The one caveat that shapes everything: Apple Silicon nested virtualization
is same-architecture only.** Your Mac's Linux VM gets an *aarch64* `/dev/kvm`,
while every FC artifact this repo owns (guest kernel config, test rootfs, the
Firecracker binary CI installs) is x86_64-only. So this rig runs an **aarch64
analog** built from the same recipes: same kernel version + same engram
fragment, upstream's aarch64 base config, upstream's aarch64 FC binary and
rootfs. A green result here validates the *recipe and the code paths*, not the
exact prod binaries — treat x86_64 CI as the final word.

Requirements: M3 or newer (nested virt is gated on it), macOS 15+,
Colima ≥ 0.8 (`vmType: vz`).

## 1. Bring up the KVM-capable VM

Use a dedicated profile so your default docker-daemon profile is untouched:

```sh
colima start --profile fc-dev --vm-type vz --nested-virtualization \
  --cpu 6 --memory 8 --disk 60
colima ssh --profile fc-dev -- ls -la /dev/kvm     # expect crw-rw---- root kvm
colima ssh --profile fc-dev -- uname -m            # aarch64
colima ssh --profile fc-dev -- sudo chmod 0666 /dev/kvm
```

Gotchas:
- `colima start` **silently switches your docker CLI context** to the new
  profile. Restore it: `docker context use colima` (or whatever your daemon
  profile is), or bundle builds will target the wrong VM.
- Work on the VM's own disk (`colima ssh`, `~/fc/`). Do NOT build through the
  virtiofs-mounted Mac home — same lesson as the bundle-build scripts' sshfs
  caveat, doubly so for kernel trees.
- Packages needed in the VM:
  `sudo apt-get install -y build-essential flex bison bc libssl-dev libelf-dev dwarves curl git squashfs-tools file`

## 2. Stage the artifacts (aarch64 analogs)

```sh
# Firecracker binary — upstream release, aarch64 flavor of what CI installs:
curl -fsSL -o fc.tgz https://github.com/firecracker-microvm/firecracker/releases/download/v1.16.0/firecracker-v1.16.0-aarch64.tgz
tar xzf fc.tgz && sudo install release-*/firecracker-v1.16.0-aarch64 /usr/local/bin/firecracker

# Rootfs — the aarch64 twin of the bucket path in
# crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh:
curl -fsSL -o ~/fc/ubuntu-22.04.ext4 \
  https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/aarch64/ubuntu-22.04.ext4
```

Guest kernel — mirror `deploy/kernel/build-fc-kernel.sh` with the aarch64 base:

```sh
# Source: 6.1.102 (same pin as build-fc-kernel.sh). kernel.org was 404ing
# tarballs when we did this; the gregkh/linux GitHub mirror tag v6.1.102 is
# byte-identical content over a different transport.
# Base config: upstream firecracker resources/guest_configs/microvm-kernel-ci-aarch64-6.1.config
# Fragment: copy deploy/kernel/engram-docker.fragment from the repo into the VM.
scripts/kconfig/merge_config.sh -m .config engram-docker.fragment
make ARCH=arm64 olddefconfig
grep -E 'CONFIG_(USER_NS|PID_NS|NET_NS|SECCOMP|SECCOMP_FILTER|NF_TABLES)=' .config  # all =y
make ARCH=arm64 -j6 Image        # ~3-4 min native; aarch64 FC boots
                                 # arch/arm64/boot/Image, NOT vmlinux
```

## 3. Boot a guest and run things in it

Minimal `vmconfig.json`: 2 vCPU, 1024-3072 MiB, the built `Image`, the rootfs
as `/dev/vda` (rw), boot args
`keep_bootcon console=ttyS0 reboot=k panic=1 root=/dev/vda rw init=/bin/bash`.

```sh
firecracker --no-api --config-file vmconfig.json          # interactive serial shell
```

Hard-won mechanics:
- **Scripted runs: never pipe the script over serial stdin** — the console
  drops/corrupts characters (`export` → `xport`). Instead loop-mount the
  rootfs, copy the script in, and boot with `init=/your-script.sh`:
  ```sh
  sudo mount -o loop ~/fc/ubuntu-22.04.ext4 /mnt && sudo cp probe.sh /mnt/ && sudo umount /mnt
  ```
- A scripted guest must end with `echo b > /proc/sysrq-trigger` or firecracker
  never exits (wrap in `timeout` as a backstop).
- With `init=/bin/bash` there is no init system: mount `/proc`, `/sys`,
  devtmpfs, a tmpfs on `/dev/shm` and `/tmp`, and — easy to forget —
  **`ip link set lo up`** (in prod that's agentd's job; without it every
  loopback probe burns its full timeout).
- The CI rootfs has ~300 MB free. Ship big payloads (the browser bundle) as a
  second read-only drive, not into the rootfs.

## 4. The validations we ran (recipes)

**Unprivileged userns / chromium-sandbox verdict** (the #569 fix-2 question) —
as uid 1000 via the launcher's exact drop:

```sh
setpriv --reuid=1000 --regid=1000 --clear-groups --inh-caps=-all --bounding-set=-all \
  unshare --user --pid --net --mount --fork true; echo rc=$?     # rc=0 = sandbox-shaped userns OK
```

**Browser-bundle smoke** (glibc fix, launcher logging, watchdog):

1. Build the arm64 bundle on the Mac (`deploy/bundles/browser/build.sh --stage <dir>`
   via the *default* colima docker), tar-pipe it into the VM
   (`tar cf - -C <dir> . | colima ssh --profile fc-dev -- 'cd ~/fc/tree && tar xf -'`),
   strip macOS `._*` AppleDouble files, `mksquashfs` it
   (`-comp zstd -all-root -noappend -no-xattrs`).
2. Attach as `/dev/vdb`; in the guest mount it at `/opt/engram/dyn/2`, symlink
   `bin/engram-browser` onto PATH (as agentd does), and run the real
   `engram-browser --ensure`.
3. Verify like prod would: read `/tmp/engram-browser.log` and
   `/tmp/engram-browser.chrome.log`, sweep `/proc/*/stat` for process states
   across two samples (stable pids vs churn), probe RFB
   (`curl telnet://127.0.0.1:5900`, expect the `RFB 003.008` banner) and CDP
   (`/json/version` via the bundle's own node — the guest has no curl in
   minimal setups).
4. Watchdog teardown test: `kill -9` Xvfb, confirm the whole uid-9000 group is
   gone within seconds and the log says
   `engram-browser: Xvfb died, tearing down the stack`, then re-run `--ensure`
   and confirm a fresh working stack.

## 5. Housekeeping

Everything lives under `~/fc/` in the `fc-dev` VM (kernel tree kept at
`~/fc/kernel-build/` for fast config tweaks; a `README-569.md` there records
the exact file inventory from the original run). `colima stop fc-dev` when
done; the profile is cheap to keep around. Remember `docker context use colima`
after any `colima start`.
