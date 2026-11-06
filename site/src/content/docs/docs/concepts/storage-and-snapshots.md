---
title: Storage and snapshots
description: Content-addressed chunks, versioned manifests, and the three places engrams gets copy-on-write for free.
sidebar:
  order: 2
---

Everything a VM is made of, its disk and its memory, is stored as chunks: fixed-size,
immutable blocks named by the hash of their content. Disk chunks are 16 MiB; memory chunks
are 512 KiB. A manifest is a versioned list of chunk hashes that describes one disk or one
memory image. Chunks live in your blob store and never change; manifests point at them.

This is the whole trick. Because a chunk's name is its content, two identical chunks are one
chunk. An image's base snapshot is written once, and every session of that image shares it.
A session's own manifest starts as a copy of the base manifest and gains new entries only
for the chunks the session wrote. A thousand sessions of a 4 GiB image take about 100 GiB in
the blob store, not 4 TiB.

## Three levels of copy-on-write

**Disk.** A VM's root filesystem is served from its manifest. Reads that hit the base image
come from shared chunks; writes produce new chunks and new manifest entries. The base
manifest never changes, so enabling an image is a one-time cost.

**Memory.** When an image is enabled, engrams captures the VM's memory once. Every session of
that image maps that canonical memory privately, and the hardware's memory management unit
does the copy-on-write: a page is copied only when a session writes to it. One copy of the
memory serves every session on the host.

**Fork.** Forking a session is a manifest copy of a few kilobytes. The forked session shares
every chunk with its parent until one of them writes.

## Where the bytes live

| Tier | What | Where | What it costs |
|---|---|---|---|
| Persistent | chunks, manifests, working-set traces | the blob store | deduplicated by construction; the base image is stored once |
| Cache | recently used chunks, assembled disk files | each host's local NVMe | bounded to a share of the disk; least recently used chunks are evicted |
| In memory | the canonical memory map plus each session's private pages | host RAM | one canonical copy per image per host |
| Metadata | sessions, event logs, manifest references, hosts | Postgres | managed and backed up like any database |

## How a VM reads its disk and memory

On Firecracker, a VM's disk is a thin volume in a device-mapper pool on the host. The kernel
owns it: no host process is in the data path, and a host agent restart does not touch a running
VM's disk. A base image is staged into the pool once per host, and every session's volume is a
snapshot of that image, so creating a session writes nothing. Guest writes land in the pool.

Memory is paged in lazily. The host agent owns a `userfaultfd` for the VM's memory region,
and on a page fault it resolves the address to a chunk, fetches it if needed, and copies the
page in. To hide that latency on resume, hosts record which chunks a session touched in its
first seconds and prefault those chunks before the VM's CPUs start.

On the macOS backend the volume is a sparse file under the host's working directory, and the
same staging and checkpoint code runs over it. Memory chunking is Firecracker-only.

## What a snapshot is

A snapshot is two manifest versions, one for disk and one for memory, plus a row in Postgres
that points at them. Taking one pauses the VM for the page-cache sync and a volume snapshot,
tens of milliseconds however much the session has written, then uploads the blocks that changed since
the previous checkpoint as chunks, writes the memory that differs from the canonical map as
chunks, records the manifests, and destroys the VM. Restoring is the reverse, on whichever
host the coordinator picks: the host stages the manifest into a fresh volume, fetching only
the chunks it does not already hold.

## Garbage collection

Chunks that no manifest references are collected by the coordinator on a schedule. Deleting
a session drops its manifests; deleting an image drops its base manifest once no session
references it. The blob store is the durable tier: as long as it holds a session's chunks,
that session can be restored on any host, including a fleet that has been replaced entirely.
