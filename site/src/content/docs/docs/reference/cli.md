---
title: CLI
description: The engrams command, its authentication model, and every subcommand.
sidebar:
  order: 1
---

`engrams` is the product CLI. It talks to the orchestrator over the same API the dashboard
uses, and nothing else; the coordinator and the hosts are internal to a deployment.

## Install

The CLI is a single static binary for Linux x86-64, Linux arm64 and macOS arm64. The
installer downloads the build for your machine from the GitHub release, checks its
checksum, and puts it on your `PATH`:

```sh
curl -fsSL https://engramsfactory.com/install.sh | sh
```

With Homebrew:

```sh
brew install cortexapps/tap/engrams
```

There are two channels. **stable** is the newest release (`0.11.0`, `0.12.0`, …). **nightly**
is a prerelease built from `main` each night, versioned after the release it precedes
(`0.12.0-rc.1`, `0.12.0-rc.2`, …). The installer takes `ENGRAMS_CHANNEL=nightly`, or an exact
version with `ENGRAMS_VERSION=0.12.0-rc.3`; Homebrew has the formula `engrams-nightly`. To
upgrade, run the installer again or `brew upgrade`.

The installer puts the binary in `/usr/local/bin` when that is writable, else in
`~/.local/bin`; `ENGRAMS_INSTALL_DIR` sets another directory. Each release also lists the
binaries and their `SHA256SUMS` on its GitHub page. To build the CLI from the repository:

```sh
cargo build --profile cli -p engram-cli   # -> target/cli/engrams
```

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
| `session shell <id>` | An interactive shell in the sandbox, in your terminal. Exit the shell to return. |
| `session logs <id> [--since <idx>]` | Tail the session's event log. `--since N` replays everything after index N first. Ctrl-C to stop. |
| `session log <id> [--limit <n>] [--from-start]` | Print the conversation timeline, newest rows first by default. |
| `session resume <id>` | Resume an idle session from its snapshot. |
| `session prompt <id> <text>` | Send a prompt. Resumes the session first if it is idle. |
| `session delete <id>` | Mark the session completed and tear down its sandbox. |

### `image`

| Command | What it does |
|---|---|
| `image list` | List the enabled images sessions may use. |
| `image enable --uri <uri> --name <name> --vcpus <n> [--description <text>] [--workdir <path>] [--memory-mib <n>] [--disk-gib <n>] [--swap-mib <n>] [--env KEY=VALUE]... [--no-wait]` | Enable an image: materialize it and capture its base snapshot. `--name` and `--vcpus` are required the first time. The warm hook is set in the dashboard. Polls the job to ready unless `--no-wait`. |
| `image config --uri <uri>` | Print an enabled image's stored config as JSON. |
| `image update --uri <uri> [the same flags] [--allow-recapture] [--no-wait]` | Change an image's config; flags you do not pass keep their stored values. Edits that change the base snapshot need `--allow-recapture`. |
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

### `tui`

`engrams tui [session-id] [--no-alt-screen] [--no-mouse] [--ax]` opens a terminal
client. With no session id it shows your sessions; with one it attaches to that session.
The transcript streams as the agent works, tool calls fold to their first lines
(Ctrl+O expands the last one), and questions and plan approvals are answered with the
number keys. `?` lists the keys.

| Key | What it does |
|---|---|
| `Enter` | Send the draft. While a run is active it is queued; `Up` pulls it back. |
| `Shift+Enter`, `Ctrl+J` | Newline in the draft. |
| `Esc` | Interrupt the run. On a draft, twice clears it. |
| `Ctrl+C` | Clear the draft; twice quits. |
| `Ctrl+D` | Back to the session list; twice quits. |
| `Ctrl+P`, `Alt+↑/↓` | Switch session. |
| `Ctrl+N` | New session: pick a profile, type the first prompt. |
| `Ctrl+\``, `Ctrl+Space` | A shell in the sandbox below the transcript. The same key hands the keys back to the agent. |
| `Ctrl+T` | Search the transcript. |
| `Ctrl+G` | Edit the draft in `$EDITOR`. |
| `PgUp`, `PgDn`, `Ctrl+Home`, `Ctrl+End` | Scroll. Scrolling to the top loads older history. |

A draft that starts with `/` is a command. Type `/` to list them; Tab completes.

| Command | What it does |
|---|---|
| `/new` | New session (the same as Ctrl+N). |
| `/sessions` | Back to the session list. |
| `/shell` | The sandbox shell. |
| `/interrupt` | Stop the current run. |
| `/resume` | Resume an idle session without sending a prompt. |
| `/transcript` | Open the loaded transcript as text in `$EDITOR`. |
| `/dump` | Write the loaded transcript to the terminal's scrollback, to read after you leave. |
| `/help`, `/quit` | The key list; leave. |

The TUI tells your terminal what the session needs from you. A terminal that supports the
[Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status)
(OSC 7501) gets one status: `working` during a run (with the agent's task list as a
percentage), `blocked` when a question, a plan approval or a sign-in waits on you, `done` or
`error` when a run ends, and `idle` otherwise. A result stays `done` until you press a key,
paste, click or scroll. Such a terminal decides how to tell you about a change, so the TUI
sends it no other alert. Other terminals see the state in the window title, and when the
terminal is not focused, a question, a finished run or a failure posts a desktop notification
(OSC 9) on iTerm2, Ghostty, Kitty, Warp and WezTerm, and rings the bell elsewhere.

Beside that status, the terminal gets one entry for each running subagent, for the sandbox
while it starts, resumes or moves, and for programs in the shell pane that report their own
status. Your other sessions are there too: each one that waits on you, runs, or finished or
failed while you were elsewhere. The TUI checks them every 5 seconds while the terminal
takes these reports or the session list is open, and a finished entry stays until you open
that session.

Flags and environment:

- `ENGRAMS_PROGRAM_STATUS=1` or `0` turns the status on or off without asking the
  terminal, for example inside a multiplexer that does not pass the reply through. `0` also
  turns off the status that `auth login` and `session exec` report while they wait.
- `--no-alt-screen` (or `ENGRAMS_TUI_NO_ALT_SCREEN=1`) keeps the terminal's own scrollback.
  Finished turns are written into it as they complete and only the composer and footer stay
  live, so your terminal's scroll, search and copy work on the transcript. History older than
  the loaded window is not paged in this mode.
- `--no-mouse` (or `ENGRAMS_TUI_NO_MOUSE=1`) leaves the mouse to the terminal, for its own
  text selection.
- `--ax` (or `ENGRAMS_TUI_AX=1`) is screen-reader mode: native scrollback, no mouse, and word
  labels (`you:`, `agent:`, `tool:`) in place of glyphs.
- `NO_COLOR` turns color off. State markers carry their meaning without it.
