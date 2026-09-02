---
title: Sandbox backends
description: Firecracker for production, Apple Virtualization for Mac development, subprocesses for iterating.
sidebar:
  order: 3
---

A backend is the driver for whatever runs a session: a microVM monitor or, in one case,
nothing at all. The rest of engrams does not know which one is in use. Three ship.

| Backend | Isolation | Snapshots | Runs on | Use it for |
|---|---|---|---|---|
| Firecracker | microVM on KVM | memory snapshot with lazy paging, chunked disk over NBD | Linux with `/dev/kvm` | production |
| Apple Virtualization | microVM on Apple's Hypervisor framework | clone of the root filesystem | macOS 12 or later on Apple Silicon | development on a Mac with real isolation |
| Process | none; a host subprocess | tarball of the working directory | macOS or Linux | iterating on the orchestration layer without a VM |

Production isolation is always Firecracker. The other two exist so the same code paths can
be exercised on a laptop; the Apple backend runs the same in-guest daemon and harness bundles
and the same image pipeline, so a session that works on a Mac works on the fleet.

`just dev` picks the backend for the machine it runs on: Firecracker when `/dev/kvm` is
readable and writable, Apple Virtualization on an Apple Silicon Mac, subprocesses otherwise.
Set `ENGRAM_SANDBOX_BACKEND` to force one.

## What a Firecracker host needs

A production host is a Linux machine with KVM. On managed Kubernetes that means a node pool
with nested virtualization, which every provider offers on Intel only: the C3 family on GKE,
m8i or bare metal on EKS. The fleet's Helm chart runs a node-prep step that does the setup
below on each node; this list is what that step does, for anyone running hosts outside
Kubernetes.

**KVM.** `/dev/kvm` must be readable and writable by the host agent's user.

```sh
[ -r /dev/kvm ] && [ -w /dev/kvm ] && echo OK || echo FAIL
```

**Lazy memory paging.** The host agent needs `/dev/userfaultfd` with mode `0666` (a udev
rule) and the sysctl `vm.unprivileged_userfaultfd = 1`.

**Chunked disks.** The kernel needs the NBD block driver (`CONFIG_BLK_DEV_NBD`, built in on
Ubuntu cloud images) loaded with enough devices, `modprobe nbd nbds_max=<N>`, and the host
agent needs `ENGRAM_NBD_DEVICES=/dev/nbd0,/dev/nbd1,...` listing the ones it may use. One
device serves one running VM.

**The guest kernel and the bundles.** Each host stages a guest kernel and the read-only
bundles for the in-guest daemon, the harnesses, and the skills. The VM boots with
`init=/sbin/engram-init`, a shim that copies the daemon out of its bundle and hands over to
it, so nothing of engrams is in your image.

**The egress proxy.** Every host runs a filtering proxy on TCP 443 and DNS on port 53, and
every VM's outbound traffic is redirected through it. It is mandatory: a host that cannot
load the proxy's certificate authority or bind its ports refuses to start. Every host in a
fleet loads the same CA pair, so a session that migrates keeps trusting its new host.

## The CPU platform is a one-way door

A base snapshot captured on a newer CPU never restores on an older one. Keep a fleet on one
platform, or bake images per platform. The Terraform quickstarts pin GKE to Sapphire Rapids
(C3) and EKS to Granite Rapids (m8i) by default; if you run both clouds and want one bake to
serve them, the AWS side offers `m7i.metal-24xl` for parity, at a much higher price.
