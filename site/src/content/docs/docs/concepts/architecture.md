---
title: Architecture
description: The four processes, what each owns, and how they talk.
sidebar:
  order: 1
---

engrams is four kinds of process and three external services. Two of the processes run in
your cluster's control plane, one runs on every host, and one runs inside every VM.

```
   dashboard · CLI · Slack · webhooks
                 │
        ┌────────▼────────┐        ┌─────────────────┐
        │   orchestrator  │───────▶│   coordinator   │────▶ Postgres
        │  auth · tasks · │        │ scheduler · idle│
        │  integrations   │        │ eviction · events│
        └─────────────────┘        └────────┬────────┘
                                            │  hosts dial in
                     ┌──────────────────────┼──────────────────────┐
              ┌──────▼──────┐        ┌──────▼──────┐        ┌──────▼──────┐
              │  host agent │        │  host agent │        │     ...     │
              │ ┌────┐┌────┐│        │ ┌────┐┌────┐│        │             │
              │ │ VM ││ VM ││        │ │ VM ││ VM ││        │             │
              │ └────┘└────┘│        │ └────┘└────┘│        │             │
              └──────┬──────┘        └──────┬──────┘        └─────────────┘
                     └──────────────┬───────┘
                          blob store (GCS · S3 · local)
                     image chunks · memory chunks · snapshots
```

## The orchestrator

The orchestrator is the product surface and the only thing a person or a browser talks to.
It owns users, sign-in, and API keys; tasks and profiles; org secrets and per-user
credentials; the Slack, Linear, GitHub, and Google Cloud integrations; pull request review;
automations; and the Connect RPC API that the dashboard and the CLI call. It serves the
dashboard's static files. It is written in TypeScript and runs on Bun.

For anything that touches a VM, the orchestrator calls the coordinator. It is the
deployment's login wall: the coordinator has no public address.

## The coordinator

The coordinator is a stateless service in front of Postgres. It schedules sessions onto
hosts, preferring the host that already holds a session's snapshot; it evicts sessions that
have gone idle; it runs the enable pipeline that turns a pushed image into a base snapshot;
it stores every session event and fans events out to subscribers; and it garbage-collects
chunks nobody references. Run as many replicas as you like: Postgres is the only authority,
and every background job is lease-guarded so replicas never do the same work twice.

## The host agent

One host agent runs on every machine that runs VMs. It dials the coordinator over HTTP,
registers, and sends a heartbeat every few seconds with its capacity and what it is running.
Hosts never need an inbound port, which is what makes a fleet behind NAT or across clouds
work.

The host agent owns everything a VM needs on the machine: the chunk cache on local NVMe, the
NBD daemon that serves chunked disks to VMs, the page-fault handler that lazily loads VM
memory from chunks, and the egress proxy that every VM's outbound traffic goes through. It
composes a sandbox backend, which is the driver for the VM technology in use, with the
harness routing that carries prompts and events between a session and the coordinator.

## The in-guest daemon

Inside every VM, a small daemon is the first process after init. It starts the harness,
runs commands and streams their output, uploads and downloads files, and opens shells. It is
not baked into your image: the host stages it in a bundle and the init shim copies it into
the VM at boot, so a fix to the daemon ships to every image at once.

The harness runs as the daemon's child. When a session resumes from a snapshot the daemon
has a clean point to respawn it, which is how a resumed agent picks up its own conversation.

## The external services

**Postgres** holds sessions and their event logs, snapshot manifests, hosts, enabled images,
registry credentials, and sealed secrets. Managed Postgres on either cloud is fine; the
requirements are version 14 or later and `LISTEN/NOTIFY`.

**The blob store** holds chunks: the immutable, hash-keyed blocks of disk and memory that
every image and snapshot is made of. GCS, S3, and S3-compatible stores such as MinIO work,
and a local directory works for development. [Storage and snapshots](../storage-and-snapshots/)
explains what lives there.

**An OCI registry** holds your session images. Any registry that `docker push` can reach
works; engrams pulls from it when you enable an image.

## The three wire surfaces

| Between | Protocol | Notes |
|---|---|---|
| clients and the orchestrator | Connect RPC over HTTP/1.1, plus SSE for event streams | the public API; see the [API overview](../../reference/api/) |
| coordinator and host agents | HTTP for registration and heartbeats, framed RPC for work | hosts dial out; a version mismatch refuses the connection |
| host agent and the VM | length-prefixed frames over vsock on Firecracker, a virtio console on Apple Virtualization | authenticated by a first-frame token |

The [engrams repository](https://github.com/cortexapps/engrams) carries the full design
document with the trait boundaries and the database schema.
