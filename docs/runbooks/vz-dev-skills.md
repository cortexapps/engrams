# Runbook: VZ dev — skill bundle edit + re-bake loop (ADR 0061)

This runbook covers the operational edit/re-bake cycle for built-in skills on the
macOS Apple Virtualization (VZ) dev stack. For the architecture and design rationale
see **ADR 0061** (`docs/adr/0061-vz-builtin-skills-erofs.md`).

## Why an extra step is needed on VZ

The coordinator resolves a selected skill name against the fleet bundle catalog —
built from the host-agent's `current_bundles` heartbeat, which it reads **once at
startup** from a `current.json` stamp in `ENGRAM_BUNDLE_DIR`. Without a staged stamp
the host heartbeats empty `current_bundles` and `POST /sessions` 400s:

> skill `skills` is unknown (not a staged fleet bundle and not in the upload catalog)

VZ also uses the **erofs** filesystem (the Kata guest kernel has `CONFIG_EROFS_FS` but
not `CONFIG_SQUASHFS`), so the FC path's squashfs bundles are not usable here.

## Enabling skills in VZ dev (first time)

`just dev` (`tilt up`) handles this automatically via the `bundles` Tilt resource:

1. **`bundles` resource** runs `just bundles-vz` before the host-agent boots.
   This builds and content-addresses each bundle as `<sha256>.erofs` under
   `var/shared/`, then writes `var/shared/current.json` (the stamp mapping
   `name → sha`).
2. **`host-agent` resource** starts after `bundles` (`resource_deps=['bundles']`),
   with `ENGRAM_BUNDLE_DIR=$PWD/var/shared`. It reads the stamp, heartbeats
   `current_bundles`, and the coordinator can now resolve enabled skills.

You can also run it manually:

```bash
just bundles-vz
```

`mkfs.erofs` is required (`brew install erofs-utils` or `nix develop`).

## Editing a built-in skill

Skill content lives under `deploy/bundles/<name>/`. After editing:

```bash
just bundles-vz
```

This re-packs the affected bundle into a new `<sha>.erofs`, rewrites
`var/shared/current.json`, and the Tilt host-agent resource (watching
`var/shared/current.json` via `deps`) auto-restarts and re-reads the stamp.

**New sessions** pick up the edited skill immediately.

**Live and resumed sessions** keep their pinned `<sha>.erofs` — the snapshot's
`spec.aux_ro_drives` recorded the original content-addressed path at session
creation, so resume is deterministic regardless of subsequent bundle edits.

> Tilt also triggers `bundles-vz` automatically on any change under
> `deploy/bundles/` (via `deps=['deploy/bundles']`), so a save in that tree is
> usually enough without a manual `just bundles-vz`.

## Changing the init shim (squashfs/erofs mount logic)

The init shim (`DEFAULT_INIT_SHIM` in `engram-image-builder`) is responsible for
mounting `spec.aux_ro_drives` at `/opt/engram/dyn/<slot>` inside the guest. It is
baked into the rootfs — **not** live-reloaded.

The init shim is baked into the guest rootfs at image-build time, not live-reloaded —
so a shim change needs a re-bake before any session can see it.

After changing the shim:

1. **Re-bake the demo image:**
   ```bash
   just bake-demo
   ```
2. **Create a fresh session** — the coordinator picks up the new image on its next
   poll cycle, so a new session immediately boots from the updated rootfs. A full
   stack restart (`just dev-down && just dev`) is not required, though you can do it
   if you want to force an immediate poll.

Existing and resumed sessions carry the shim that was baked when they were created
and will not pick up the change.

## Quick reference

| What changed | Action needed | Picks up in |
|---|---|---|
| Skill content (`deploy/bundles/<name>/`) | `just bundles-vz` (or save in that tree — Tilt re-runs) | New sessions |
| Bundle list / stamp format | `just bundles-vz` | New sessions |
| Init shim mount logic (`engram-image-builder`) | `just bake-demo` + fresh session | New sessions only |
| Guest harness (`engram-harness-claude`) | `just bake-demo` + fresh session | New sessions only |
