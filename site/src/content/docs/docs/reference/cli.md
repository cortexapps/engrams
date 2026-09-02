---
title: CLI
description: The engrams command, its authentication model, and every subcommand.
sidebar:
  order: 1
---

`engrams` is the product CLI. It talks to the orchestrator over the same API the dashboard
uses, and nothing else; the coordinator and the hosts are internal to a deployment.

## Install

The CLI is a single binary built with Bun from the `cli/` directory of the repository:

```sh
cd cli && bun install && bun run build   # -> cli/dist/engrams
```

Copy `cli/dist/engrams` somewhere on your `PATH`.

## Authentication

The CLI follows the `gh` model. The API key comes from, in order:

1. `ENGRAMS_API_KEY` in the environment. For CI and scripts; never written to disk.
2. The key stored for the active host in `~/.config/engrams/hosts.json`, written by
   `engrams auth login`.

`engrams auth login` runs a device-code flow: it prints a one-time code, you approve it in the
dashboard, and the CLI stores a durable personal API key under the host's URL. The file is
mode 0600 and keyed by host, so one machine can hold credentials for a production deployment
and a local stack side by side. `engrams auth logout` revokes the key and forgets it.

The host is `--url`, then `ENGRAMS_URL`, then the CLI's built-in default. Set `ENGRAMS_URL` to
your deployment's URL; for the local stack it is `http://localhost:8787`.

## Output

Every command prints a human-readable table by default. `--json` prints pretty JSON on stdout
and nothing else, for `jq` and scripts. Errors go to stderr as `engrams: …` with exit code 1.
`session exec` exits with the remote command's exit code.

## Commands

Global flags: `--url <url>`, `--json`.

### `auth`

| Command | What it does |
|---|---|
| `auth login` | Authenticate through the browser and store an API key for this host. |
| `auth logout` | Revoke the stored key for this host and forget it. |
| `auth status` | Show who you are logged in as. |

### `task`

Tasks are the product-level unit of work: a profile plus a prompt.

| Command | What it does |
|---|---|
| `task create --profile <name-or-id> [--prompt <text>] [--title <text>] [--harness <name>] [--model <name>] [--effort <level>] [--mode <mode>]` | Start a task from a profile. Prints the primary session id. The overrides replace the profile's harness, model, and reasoning effort; `--mode plan` starts the first turn in plan mode. |
| `task list` | List your tasks. Admins also see sessions that belong to no task. |
| `task get <id>` | Print one task with its sessions. |
| `task delete <id>` | Delete a task and tear down its sessions. |

### `session`

| Command | What it does |
|---|---|
| `session create --image <uri> [--harness <name>] [--prompt <text>] [--dev-vm]` | Boot a session from a raw image URI with no profile. An admin's escape hatch. `--dev-vm` starts a VM with no harness, for use through `exec`. |
| `session list` | List sessions. |
| `session get <id>` | Print one session. |
| `session exec <id> <cmd> [--timeout-secs <n>]` | Run a shell command in the sandbox, streaming its output, and exit with its exit code. |
| `session logs <id> [--since <idx>]` | Tail the session's event log. `--since N` replays everything after index N first. Ctrl-C to stop. |
| `session log <id> [--limit <n>] [--from-start]` | Print the conversation timeline, newest rows first by default. |
| `session resume <id>` | Resume an idle session from its snapshot. |
| `session prompt <id> <text>` | Send a prompt. Resumes the session first if it is idle. |
| `session delete <id>` | Mark the session completed and tear down its sandbox. |

### `image`

| Command | What it does |
|---|---|
| `image list` | List the enabled images sessions may use. |
| `image enable --uri <uri> [--config <path>] [--no-wait]` | Enable an image: materialize it and capture its base snapshot. The [image config](../image-config/) is required the first time. Polls the job to ready unless `--no-wait`. |
| `image config --uri <uri>` | Print an enabled image's stored config as TOML. |
| `image update --uri <uri> --config <path> [--allow-recapture] [--no-wait]` | Replace an image's config. Edits that change the base snapshot need `--allow-recapture`. |
| `image disable --uri <uri>` | Disable an image. The registry artifact is untouched. |
| `image refresh --uri <uri> [--recapture]` | Re-fetch a tag that moved, and optionally force a new capture. |
| `image jobs` | List recent enable and refresh jobs. |
| `image job <id>` | Print one job, including the warm hook's current stage. |
| `image poll-job <id>` | Poll a job until it is ready or failed. |
| `image retry-job <id> [--no-wait]` | Re-queue a failed job. |

### `registry`

Registry credentials are envelope-encrypted on the server and never returned.

| Command | What it does |
|---|---|
| `registry add --host <host> [--auth-kind static\|gcp-workload-identity] [--username <name>] [--password-file <path>] [--password-stdin] [--impersonate-sa <email>]` | Add or update the credential for a registry host. |
| `registry list` | List registry credentials, with passwords masked. |
| `registry rm <host>` | Remove a credential. |

### `host`

Fleet operations, admin only.

| Command | What it does |
|---|---|
| `host list` | List hosts with their capacity. |
| `host get <id>` | Print one host. |
| `host drain <id>` | Mark a host draining; new sessions avoid it. |
| `host uncordon <id>` | Clear a cordon; the host returns to ready. |
| `host evacuate <session-id>` | Move one session off its host; it resumes on a peer. |
| `host delete <id>` | Deregister a host. Refused while sessions are bound to it. |

### `profile`

| Command | What it does |
|---|---|
| `profile list` | List the profiles you can launch. |
| `profile get <id>` | Print one profile. |

Profiles are created and edited in the dashboard.

### `apikey`

Global service-account keys, admin only. Your own login key is managed by `auth`.

| Command | What it does |
|---|---|
| `apikey create --name <name> [--role admin\|user] [--expires-at <iso>]` | Mint a key. The plaintext is printed once, alone, on stdout. |
| `apikey list` | List keys with a masked preview. |
| `apikey revoke <id>` | Revoke a key. |

### `admin`

Explicit triggers for things engrams otherwise does on its own schedule.

| Command | What it does |
|---|---|
| `admin flush <session-id>` | Flush a session's dirty disk chunks now. |
| `admin evict-idle <session-id>` | Run the idle-eviction pipeline for one session now. The session ends idle. |
| `admin gc [--apply] [--grace-secs <n>]` | Run the blob garbage-collection sweeps. A report only, unless `--apply`. |
