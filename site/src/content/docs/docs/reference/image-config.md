---
title: Image config
description: Every field of the TOML file you pass when you enable an image.
sidebar:
  order: 3
---

An image config is a TOML file passed to `engrams image enable --config` the first time an
image is enabled, and to `engrams image update --config` after that. engrams stores it; it
is never part of the image. Unknown keys are an error, so a typo fails loudly rather than
being ignored.

```toml
name = "team-api"
description = "The backend API service"
workdir = "/workspace"

[env]
NODE_ENV = "development"
JAVA_HOME = "/opt/java"

[resources]
suggested_vcpus = 2
suggested_memory_mib = 4096
suggested_disk_gib = 20
suggested_swap_mib = 1024

[warm]
command = ["./warm.sh"]
timeout_secs = 600
workdir = "/workspace"

[warm.network]
default = "deny"
allow_hosts = ["repo1.maven.org"]
allow_host_patterns = ["*.githubusercontent.com"]

[[warm.env]]
name = "GRADLE_OPTS"
value = { kind = "literal", value = "-Dorg.gradle.daemon=true" }

[[warm.env]]
name = "NPM_TOKEN"
value = { kind = "secret_ref", secret_ref = "NPM_TOKEN" }
```

## Top level

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | The display name in the dashboard and the CLI. |
| `description` | no | Free text. |
| `env` | no | Environment variables for every session of this image. The Dockerfile's `ENV` is read at enable time and merged underneath, so a key here overrides the same key in the image, and a value the session supplies overrides both. |
| `workdir` | no | The working directory for the harness and for `engrams session exec`. Defaults to the Dockerfile's `WORKDIR`, then to `/`. The directory must exist in the image; a missing one fails the spawn. |
| `resources` | see below | The VM's size. |
| `warm` | no | A command run once at enable time. See [Authoring a warm hook](../../guides/warm-hooks/). |

What a session may reach on the network and which secrets it holds are not here. They are
session policy, set on the profile, so one image can serve profiles with different postures.

## `[resources]`

| Field | Required | Meaning |
|---|---|---|
| `suggested_vcpus` | yes | The guest's vCPU count. Enabling rejects an image without it, because placement reserves it against a host's CPU budget. |
| `suggested_memory_mib` | no | Guest memory in MiB. |
| `suggested_disk_gib` | no | The session's disk budget in GiB. It feeds placement and sets the floor for the root filesystem's size at enable time; without it the filesystem is sized to its content. A change takes effect at the next capture, not on live sessions. |
| `suggested_swap_mib` | no | An ephemeral swap device in MiB. Absent or 0 means none. Swap is discarded at every snapshot, so the value also bounds how long a snapshot can take. A good default is a quarter of memory, clamped between 1 GiB and 8 GiB; a value larger than memory is rejected. |

Changing memory, vCPUs, disk, or swap changes the base snapshot, so `engrams image update`
requires `--allow-recapture` for those edits and re-captures the image.

## `[warm]`

| Field | Required | Meaning |
|---|---|---|
| `command` | yes | The command as an argv array. It runs inside the capture VM once the in-guest daemon is ready and before the snapshot is frozen. It must exit; anything it leaves running is captured live. |
| `timeout_secs` | no, default 600 | The global deadline. The in-guest daemon kills the command at this point and the capture fails. |
| `workdir` | no | Working directory for the command. Defaults to the top-level `workdir`. |
| `env` | no | Capture-time environment entries, each a `name` and a `value`. A value is either `{ kind = "literal", value = "…" }` or `{ kind = "secret_ref", secret_ref = "…" }`. Secret references are resolved at capture through the same store sessions use and are never stored resolved; an unresolvable reference fails the capture. |
| `network` | no | Network policy for the capture VM while the command runs. Absent means no network at all. |

### `[warm.network]`

| Field | Meaning |
|---|---|
| `default` | `"deny"` (the default) or `"allow"`. `"allow"` is the development posture: no agent runs at capture, so the session threat model does not apply. |
| `allow_hosts` | Exact hostnames the command may reach when `default` is `"deny"`. |
| `allow_host_patterns` | Glob patterns matched against the hostname, such as `*.githubusercontent.com`. |

Everything under `[warm]` is captured into the base snapshot, so any change to it means a
recapture, and `engrams image update` asks for `--allow-recapture`.

## Reading a stored config back

```sh
engrams image config --uri registry.example.com/team/api:warm-1
```

This prints the config engrams holds for an enabled image as TOML, which is the right
starting point for an edit.
