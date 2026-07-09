# 0082 — Tar-input materialization: metadata flows tar → ext4 as data, never through host inodes

Status: Proposed

Builds on: ADR 0080 (host-side materialization — the pipeline this
fixes), ADR 0036 (byte-deterministic ext4 packs — the pin this
leans on).

## Problem

The ADR 0080 materializer packs the flattened OCI tree with
`mke2fs -d <dir>`, which copies uid/gid/mode from the *host
filesystem's* inodes. That makes the host FS the metadata authority —
and the host FS censors metadata the calling process isn't privileged
to store:

- An unprivileged materializer (macOS/VZ dev: the host-agent runs as
  the user — the Tiltfile is explicit, "No sudo on macOS") cannot
  chown scratch-tree files to uid 0. Every inode in the packed image
  is owned by the build user.
- The killer interaction: modes *are* preserved, so `/usr/bin/mount`
  ships `04755` **owned by uid 501**. In-guest PID 1 (root) execs it,
  setuid drops euid to 501 and clears the effective capability set,
  and **every early mount silently fails** (`2>/dev/null || true`):
  no `/proc` (every boot mark reads `uptime=?`), no `/dev`, so no
  `/dev/vd*`, so no agentd bundle → `engram-init: FATAL` → kernel
  panic. Proven 2026-07-09: broken image `607e90b6` (`mount` =
  04755/uid 501) vs working image `382da14f` (identical modes, uid 0)
  differ *only* in ownership.
- The rootful path (Linux prod, `sudo` host-agent) works, but only
  because root may store the metadata — so dev and prod exercise
  different mechanisms and produce byte-different images (zero chunk
  dedup across them), and the unprivileged path ships a latently
  broken image class (setuid-to-builder binaries).

An interim fix (offline `debugfs` repair of the packed image) works
but adds a second ownership mechanism, a second binary-resolution
cascade, and a diff-based stamp whose output depends on host-FS
quirks. This ADR replaces it before it lands.

## Decision

**Feed mke2fs a tarball, not a directory.** e2fsprogs ≥ 1.47.1
(libarchive-enabled) accepts `-d <tar>` and writes the tar headers'
uid/gid/mode/mtime/xattrs straight into ext4 inodes — plain data the
whole way, no privilege required at any step, verified against the
prod-exact binary (debian:trixie e2fsprogs 1.47.2 + `libarchive13t64`,
running as `nobody`: setuid+uid0, hardlinks (same inode), symlinks,
FIFOs, and `security.capability` xattrs all land correctly).

Pipeline:

1. **Flatten** (unchanged): extract layers, apply whiteouts/opaque
   dirs — the tree stores *contents*; `TreeMetadata` records the
   tar-carried truth (as today).
2. **Emit** (new): walk tree + sidecar and write one deterministic
   tar — headers from the sidecar (uid/gid/mode), mtimes clamped at
   emit (retires the tree-mutating `clamp_mtimes` pass), hardlinks
   preserved via a (dev,ino) map, xattrs as PAX `SCHILY.xattr.*`
   records, `/sbin/engram-init` appended as a root-owned entry
   (folds `inject_init`'s metadata concern in). The tree is deleted
   after emit, so peak scratch stays ~2× image size (tar replaces the
   tree in the peak; `-d --` stdin is not supported by 1.47.2 — needs
   a temp file).
3. **Pack** (changed): `mke2fs -d <tar>`. Delete
   `TreeMetadata::apply_ownership` and its PermissionDenied fork, the
   debugfs stamping module, `resolve_debugfs`, and the macOS
   setuid-strip test relaxation. One path: root and unprivileged,
   Linux and macOS, byte-identical output.

Toolchain (the reason this wasn't done in 0080 — all three blockers
turned out soft):

- **flake.nix**: e2fsprogs stays pinned at 1.47.2 but built with
  libarchive; delete the dead `mke2fs-static` output (its only
  consumer, `cli-tools`, was retired by ADR 0080 — which also retires
  the pin-rev's "libarchive breaks the static-musl link" reason).
- **host-agent.Dockerfile**: apt adds `libarchive13t64`; the existing
  mke2fs version gate grows a build-time tar-input smoke (pack a tiny
  tar, assert uid 0 lands) so a base-image regression fails the build,
  not a guest boot.
- **Tiltfile**: drop the Homebrew e2fsprogs PATH prepend — Homebrew
  compiles `--without-libarchive` and would shadow the flake's mke2fs.

## Consequences

- Ownership/setuid correctness no longer depends on who runs the
  materializer. The VZ dev panic class is structurally gone.
- Dev- and prod-materialized images of the same OCI image become
  byte-identical → cross-host chunk dedup returns; determinism
  improves (host-FS timestamps/quirks can no longer leak in).
- Prod image grows one apt package (libarchive). CI's
  `setup-reproducible-mke2fs` action inherits the flake override.
- The interim debugfs stamp (uncommitted) is superseded and dropped.

## As-built notes (verified against e2fsprogs 1.47.2, debian:trixie + the flake build)

- **Parents must precede children in the tar** —
  `__populate_fs_from_tar` does not create implicit directories
  (errors `cannot find directory … to create …`). The emitter's
  sorted-path ordering guarantees this (a parent path is a strict
  prefix of its children); the Dockerfile gate tars `.`/`./bin`
  explicitly for the same reason.
- **xattr whitelist**: `set_inode_xattr_tar` in
  `create_inode_libarchive.c` applies ONLY `security.capability`
  (and Hurd's `gnu.translator`) — `user.*`/`security.selinux`/
  `trusted.*` are dropped upstream-by-design. File capabilities (the
  one namespace guest binaries need) survive — pinned in the
  integration test. The materializer warns (never silently drops)
  when an image carries xattrs outside the whitelist.
- **Device nodes and FIFOs now materialize**: tar headers carry
  devmajor/devminor and mke2fs writes them without privilege
  (verified: char 1:3, block 7:0) — the dir-mode path could never
  `mknod` unprivileged, so this is a fidelity *gain*. Flatten records
  extraction-skipped specials in the sidecar and the emitter appends
  them.
- **`-d --` (stdin) does not exist in 1.47.2** — the emit writes a
  temp tar; the tree is deleted before pack so peak scratch stays
  tree+tar → tar+ext4.
- An earlier probe blaming PAX format for a lost setuid bit was a
  test artifact (tar missing parent-dir entries); with parents
  present, GNU and PAX headers both round-trip `04755` + uid 0.
- Emit format: GNU headers, PAX `SCHILY.xattr.*` extensions only on
  entries that carry xattrs.
