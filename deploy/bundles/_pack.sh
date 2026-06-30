#!/usr/bin/env bash
# Shared reproducible squashfs pack for the fleet RO bundles (ADR 0027 / 0035).
#
# Content-addressing REQUIRES that identical bundle content yields an IDENTICAL
# sha256 across builds. The same bundle is built independently in more than one
# place — the FC-host image bake (`just bundles-squashfs`) and the e2e/test stage
# (ci.yml "Stage RO skill bundles") — and the coordinator resolves a session's
# selected bundle by the host-reported sha. If two builds of the same content
# disagree on the sha, the coordinator asks a host for a sha it never staged and
# restore fails with "selected skill <sha> … is not staged on this host (catalog
# materialize gap?)".
#
# mksquashfs is NOT reproducible by default — it stamps the filesystem creation
# time into the superblock, and `cp -R` in the callers' stage_tree resets every
# file's mtime to "now", which mksquashfs then embeds per inode. SOURCE_DATE_EPOCH
# clamps BOTH (superblock + every file's timestamp) to a fixed value, so the digest
# depends ONLY on the content — the reproducible-builds standard, matching the
# server-side packer at crates/engram-coordinator/src/skill_pack.rs (which packs
# uploaded skills + the harness catalog the same way, so a bundle built by either
# path content-addresses identically).

# pack_squashfs <tree> <out>: pack <tree> into a reproducible squashfs at <out>.
pack_squashfs() {
    local tree="$1" out="$2"
    rm -f "$out"
    # -all-root: the guest mounts RO as root. -no-xattrs: none needed. Do NOT also
    # pass -mkfs-time/-all-time — mksquashfs refuses both alongside SOURCE_DATE_EPOCH.
    SOURCE_DATE_EPOCH=0 mksquashfs "$tree" "$out" \
        -comp zstd -all-root -noappend -no-xattrs >/dev/null
}
