# Changelog

This file records the notable changes in each release of engrams. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the version numbers follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

engrams is a 0.x project, so a minor release can contain breaking changes. Each release
starts with **Upgrade notes**: read them before you upgrade. The upgrade procedure is in
[Releases and upgrades](docs/deploy.md#releases-and-upgrades).

## [Unreleased]

## [0.13.0] - 2026-11-11

### Upgrade notes

- **The upgrade drains every host.** The new host agent cannot adopt a VM from an earlier
  build. Before `helm upgrade` of either chart changes anything, a job cordons every host
  and evicts each running session to a checkpoint. Each session resumes the next time it is
  used. A session the job cannot evict within `upgradeDrain.timeoutSeconds` stops the
  upgrade. The drain is safe to repeat: if it fails with a database error about a missing
  column, run the same `helm upgrade` again. With `upgradeDrain.enabled: false`, evict
  every session before the upgrade. To roll back past this release, drain the hosts first
  the same way. ([#1734], [#1778], [#1783])
- **Firecracker hosts need a session-disk pool and two kernel modules.** Node-prep in the
  `engram-host-fleet` chart builds a device-mapper thin pool and loads `dm_thin_pool` and
  `ublk_drv`; the node also needs `thin-provisioning-tools`. With node-prep disabled, the
  node image must provide the pool (`ENGRAM_THIN_POOL`). The host agent does not start the
  Firecracker backend without a pool. Without `/dev/ublk-control`, it writes each disk in
  full before its VM starts. ([#1733], [#1783])
- **Node-prep makes room for the pool.** When the pool would leave less than 15% of the
  disk free, node-prep clears the chunk cache first, so the first resumes on that host are
  slower. A disk that is still too small fails node-prep with no change.
  `storage.chunkCacheDiskFraction` now defaults to `0.3`. ([#1744])
- **Three database migrations (0126, 0127, 0128).** Either chart can be upgraded first.
  Migration 0126 marks dead an idle session whose only recoverable state was the host's
  live disk manifest.
- **With the operator enabled, set `operator.capacityFloor` below the host count** (`0` for
  a single host). The operator replaces a host pod only when the fleet keeps that many
  Ready hosts without it.
- **Removed.** Delete these from your values, environment and alert policies:
  - The `nbd` chart values, `ENGRAM_NBD_MAX_SLOTS` and `ENGRAM_NBD_WARM_SLOTS`.
  - `firecracker.restoreMode` and `ENGRAM_FC_RESTORE_MODE`. Every resume restores memory
    lazily, so the host image must ship `engram-uffd-handler`.
  - `ENGRAM_QUARANTINE_CAPTURE_TIMEOUT_SECS`, and the metrics
    `engram_quarantine_stuck_total` and `engram_durability_rollback_total`.
  - `engrams admin flush` and the `FlushSession` RPC.
- **Fleet API.** `HostView` replaces `nbd_slots_total` and `nbd_slots_in_use` with
  `pool_data_free_mib`, `pool_data_total_mib`, `pool_metadata_free_mib` and
  `pool_metadata_total_mib`. The capability vector reports `session_disk` where it
  reported `nbd`.
- **The first enable or refresh of each image uploads it in full and captures again.** The
  new disk layout shares no chunks with images packed before. Sessions, checkpoints and
  base snapshots that exist are not changed. ([#1759])
- **The Slack app needs new scopes.** The bot token asks for `im:write`, `mpim:write` and
  `groups:write`. Update the app from the manifest and install it again. ([#1702])
- **The orchestrator logs JSON by default.** Set `ENGRAM_LOG_FORMAT=pretty` for the old
  format. ([#1753])
- **Save your GitHub token again** in Settings → Credentials before you use the "Mine"
  filter on Reviews. A token saved earlier names no login. ([#1719])
- **To build from source,** you need Node 26 and Rust 1.94 or later. ([#1736], [#1573])

### Added

- **An automation is an agent with ways in.** A definition has one agent, scoped per
  event, per key (a thread, a pull request, an issue) or per automation. Each way in
  starts the agent, reaches it, or closes it. The Slack threads built-in and the web
  canvas use this model. ([#1694], [#1695], [#1700])
- Connectors for Vanta, Rippling and Google Workspace. A connector can get its token with
  a client-credentials grant or a service-account grant. ([#1702], [#1703])
- The Slack connector opens direct messages and private channels, finds a member by
  email, and creates and invites to channels. ([#1702])
- A "Programmatic only" profile setting. Only automations, API keys and admins can start
  sessions on such a profile. ([#1713])
- A configurable API region for OpenRouter, for workspaces that permit only regional
  hosts. ([#1714])
- An All / Mine switch on Reviews. ([#1719])
- `storage.ioScheduler` (`engram-host-fleet`, default `none`): node-prep sets the I/O
  scheduler of the work-dir disk. ([#1766])

### Changed

- **The kernel owns the session disk.** A VM's disk is a thin volume in a device-mapper
  pool, and a host agent restart does not touch a running VM's disk. A create writes
  nothing. A resume starts the VM first and copies the missing part of the disk in behind
  it, so the time to the first prompt does not grow with the disk size. A checkpoint
  pauses the VM for tens of milliseconds and uploads only the changed blocks. ([#1733],
  [#1783])
- One chunk fill engine per host serves memory and disk restores, with one priority order
  and one shared limit. ([#1764])
- Images are packed with a layout that keeps most disk chunks stable from one version to
  the next, so a new version uploads and stages about a third of the data. An image with
  no suggested disk size gets a disk of 4× its data. ([#1759])
- Placement refuses a host whose pool has less than `ENGRAM_PLACEMENT_POOL_FLOOR_BYTES`
  (16 GiB by default) of data space or 64 MiB of metadata space.
- A parked VM keeps its host until a queued session has no other room. ([#1782])
- A prompt does not wait for a checkpoint upload. ([#1758])
- Destroying a VM does not wait for a guest shutdown. ([#1755])
- The orchestrator caches a verified API key for 5 s. A revoked key can work for up to
  5 s on another replica. ([#1753])
- Dependency updates.

### Fixed

- A live move whose destination never loaded the state ended the session. The source now
  resumes. ([#1750])
- A resume could wait in the queue while a host had room, after the image's memory config
  grew. ([#1789])
- A live move of a large session failed on a request above 4 MiB. ([#1788])
- A rate-limited upload to the chunk store retried after 300 ms. It now backs off for
  seconds. ([#1784])
- A session could die on resume after a second base capture of its image. ([#1730])
- A deploy could evict sessions that had survived the host agent restart. ([#1728])
- The coordinator kept dialing a host at an old address. ([#1745])
- Scheduled automations: re-enable and "Run now" work for a cron way in. ([#1716],
  [#1717])
- The Slack bot reads group direct messages. ([#1709])
- The `vanta` CLI sends list filters as repeated parameters. ([#1711])
- The TUI cursor stays on its row when a space ends a wrapped row. ([#1693])

## [0.12.0] - 2026-10-26

### Upgrade notes

- **The server upgrade changes only the images.** This release has no database migrations, no
  chart value changes, no Terraform changes, and no change to the host wire version. Run
  `helm upgrade` on both releases in the usual sequence. You can roll back to 0.11.0.
- **The `engrams` CLI is a Rust binary.** The release assets keep their names
  (`engrams-linux-x64`, `engrams-linux-arm64`, `engrams-darwin-arm64`), and the commands,
  flags, `--json` shapes, `hosts.json` credential file, and exit codes do not change. Build it
  from the repository with `cargo build --profile cli -p engram-cli`; the `cli/` directory and
  its Bun toolchain are gone. ([#1642], [#1644], [#1645])
- **The token usage attributes on chat spans changed.** They now use the GenAI semantic
  convention keys. If a dashboard or a query reads the old keys, change it:
  - `gen_ai.usage.cache_read_input_tokens` is now `gen_ai.usage.cache_read.input_tokens`.
  - `gen_ai.usage.cache_creation_input_tokens` is now
    `gen_ai.usage.cache_creation.input_tokens`.
  - `gen_ai.usage.input_tokens` is now the total input, including cache tokens. For the
    Claude harness, it was the uncached input only. ([#1675])

### Added

- **`engrams tui`**, a terminal client for sessions. It shows the session list and the
  transcript with Markdown, tables, diffs, and syntax highlighting. You can send prompts,
  answer questions, approve plans, search the transcript (`Ctrl+T`), edit a draft in
  `$EDITOR` (`Ctrl+G`), and select text to copy it. Use `--no-alt-screen` to keep the
  terminal scrollback and `--ax` for screen readers. ([#1658], [#1664])
- The TUI and the CLI report the session state to the terminal through the Program Status
  Protocol (OSC 7501): working, waiting for you, done, or failed. The TUI also reports your
  other sessions. `ENGRAMS_PROGRAM_STATUS=0` turns it off. ([#1663])
- An installer for the CLI (`curl -fsSL https://engramsfactory.com/install.sh | sh`), the
  Homebrew formulae `cortexapps/tap/engrams` and `cortexapps/tap/engrams-nightly`, and a
  nightly prerelease channel (`v<next>-rc.<n>`) built from `main`. ([#1660], [#1661])
- Chat and tool spans carry `gen_ai.conversation.id` (the session id). ([#1675])

### Changed

- The `engrams` CLI moved from Bun to Rust (`crates/engram-cli` on the new
  `crates/engram-app-client` library). The binary is 3 MB and uses about 8 MB of memory.
  ([#1644])
- The TUI colours adapt to the terminal background and pass WCAG AA contrast. De-emphasized
  text uses the terminal DIM attribute, not a grey colour; `ENGRAMS_TUI_NO_DIM=1` turns it
  off. ([#1670])
- When a session starts without an integration because the person has not connected their
  personal credential, the agent is told which integration is off and how to connect it.
  Before, only the web start screen showed this. ([#1671])
- Updates to the AWS SDK, parking_lot 0.13, zip 9, `@hookform/resolvers` 6, and other
  dependencies.

### Fixed

- A warm base capture could fail with `pre-capture guest sync exited Some(1)` when a command
  in the guest wrote its output while the capture cleared the exec journal. ([#1672])
- The CLI sends `Content-Length` on every request. Without it, a front end that requires the
  header refused requests with no fields (HTTP 411), for example `auth status` and
  `task list`. ([#1652])
- `engrams auth login` and `engrams auth status` show why a key could not be verified.
  ([#1654])
- `engrams session logs` printed the event-stream envelope instead of the event payload.
  ([#1644])
- The CLI help no longer refers to a design record. ([#1662])

## [0.11.0] - 2026-10-19

### Upgrade notes

- **Idle sessions from 0.10.0 lose their running processes on resume.** The swap drive of a
  guest is now a chunked NBD disk, and each snapshot contains it. A memory snapshot from
  0.10.0 has no swap manifest, so 0.11.0 does not restore it. An Idle session with such a
  snapshot resumes by a disk-only cold boot: its files stay, its running processes stop, and
  the session shows a "resumed from disk" marker. You do not have to migrate data.
  ([#1601], [#1603], [#1613])
- **You cannot roll back to 0.10.0.** Coordinator migration 0124 removes a column that 0.10.0
  reads. Back up the coordinator and orchestrator databases before you upgrade.
- **Upgrade the coordinator and all hosts together.** The host wire version changes from 28
  to 37, and the coordinator accepts only an exact match. During the roll, the coordinator
  does not place sessions on old hosts, so sessions wait for upgraded hosts.
- **Recreate the host nodes.** In the `engram-host-fleet` chart, the defaults of
  `nbd.nbdsMax` and `nbd.warmSlots` change from 64 to 128, because a guest with swap uses two
  NBD devices. The kernel reads `nbds_max` only when it loads the module, so the new value
  applies only to new nodes. If you set these values, set them to two times the number of
  guests on a node, or more. ([#1607])
- **Upgrade the host-fleet operator when no scale-down wave runs.** The operator now marks a
  victim node with the annotation `fleet.engram.io/victim` and does not read the old
  `scaledown-victim` annotation. ([#1588])
- **`operator.scaler` must be `noop`, `gke`, or `asg`.** An unknown value now stops the
  operator at startup. A `gke` or `asg` scaler that cannot start also stops the operator; in
  0.10.0 it changed to a no-op scaler that continued to cordon and delete hosts. `noop` now
  only calculates and logs plans. ([#1583])
- **Remove these chart values.** They have no effect now:
  - `firecracker.freshRestoreMode` (`engram-host-fleet`): a fresh create always restores from
    the image memory file. ([#1596])
  - `operator.autoscaling.maxConcurrentDrains` (`engram-host-fleet`): `maxShedPerWave` is the
    only limit. ([#1588])
  - `orchestrator.killSwitches` (`engram`): the legacy Slack thread engine is gone. ([#1575])
- **Stored model selections change to model families.** Orchestrator migrations 0095 and
  0096 change the model ids in profiles, tasks, and automation session blocks to family ids.
  Claude: `opus` (default), `fable`, `sonnet`, `haiku`. Codex: `sol` (default), `astra`,
  `luna`. If you set a model id in a script or an API call, use a family id. ([#1608])
- **API changes.** On the coordinator, `TeleportSession` replaces `EvacuateSession`, and the
  `SnapshotWait` RPC is gone. On the orchestrator, `ReviewService` no longer has
  `ListEnrollments`, `UpsertEnrollment`, or `DeleteEnrollment`; the `repos` input of the PR
  review automation controls enrollment. The `ENGRAM_LIVE_TELEPORT` and
  `ORCHESTRATOR_SLACK_AUTOMATION_DISABLED` environment variables are gone.
  ([#1591], [#1599], [#1574], [#1575])
- The Terraform modules did not change.

### Added

- **Durable session teleport.** `TeleportSession` moves a session to a different host. A state
  machine in Postgres controls the move, so a coordinator restart does not stop it.
  `RetireHost` moves the sessions off a host and gives a retirement grant only when the host
  is empty. The operator removes a node only after the coordinator gives the grant.
  ([#1582], [#1585], [#1588], [#1590], [#1591])
- **Harness continuity across a move.** The harness sequences its events, and the host
  acknowledges an event only after the coordinator stores it. A harness that continues after
  a snapshot attaches again at the new binding generation and finishes its run.
  ([#1586], [#1587], [#1589])
- **Live moves of guests with swap.** Post-copy now moves the swap disk too. ([#1605])
- **Faster post-copy.** A demand fault fetches a window of chunks (`ENGRAM_UFFD_FAULT_AROUND`,
  default 8). Each post-copy source sends a hot set from the kernel idle-page bits, so the
  destination fetches the pages that the guest uses first. ([#1611], [#1612])
- **NBD slots in placement.** Hosts report their NBD slots, and placement does not put a
  session on a host that has too few. ([#1607])
- **Retention policy.** Admins set how long to keep run details (7 to 365 days, default 30) in
  **Settings → Retention**. The sweep deletes old run step details and finished DBOS
  workflows. ([#1578])
- **`decide` and `list_profiles` automation blocks.** `decide` asks typed questions through
  the OpenRouter Decisions API and returns calibrated answers. Without an OpenRouter key, it
  returns `decided: false`. ([#1617])
- **Smart profile routing in Slack.** Set the `routing` input of the Slack automation to
  `smart`, and a new thread goes to the profile that matches the request. When the model is
  not sure, it asks the person to choose. Channel overrides always apply first. ([#1619])
- `CONTRIBUTING.md`, `SECURITY.md`, and a CLA check on pull requests. ([#1471], [#1581])

### Changed

- The harness model options are model families, so a new model release does not change the
  option ids. The bundled Claude Code CLI is 2.1.285 and the Codex CLI is 0.161.0. ([#1608])
- The listener of a parked session stops until the session resumes. ([#1579])
- A PR review keeps one status comment for each pull request, and it posts a GitHub review
  only when it has findings. ([#1577])
- The `repos` input of the PR review automation is the only record of enrolled
  repositories. ([#1574])
- Snapshot metadata is stored as JSON. ([#1601])
- A host uses one NBD device for each role of each sandbox. ([#1602])
- Updates to the OpenTelemetry, AWS SDK, React, and other dependencies, including
  framer-motion 14.

### Removed

- The C1 stop-and-copy migration path. Each live move uses post-copy. ([#1599])
- The legacy Slack thread engine, its LLM profile picker, and the `slack_session`
  table. ([#1575])
- The `review_enrollment` table. ([#1574])
- The raw sparse swap file and the swap disarm steps before a capture. ([#1603])

### Fixed

- A memory image that a host cannot restore now causes a disk-only cold boot. Before, the
  session became `Dead` after five failed resumes. ([#1613])
- A guest that stops before the agent is ready now fails at once and shows the end of the
  console log. Before, it waited 180 seconds. ([#1610])
- A Slack thread has no time limit for a turn. When a run reaches its limit, it pauses and
  does not fail. ([#1568])
- The session of a Slack thread gets the Slack system prompt again, so answers use Slack
  formatting. ([#1576])
- The operator no longer decreases the cloud target size of a node pool when it adds
  nodes. ([#1583])
- The host-agent keeps all dirty data and sandbox state through seal, restore, teardown, and
  capture. ([#1584])
- A live move no longer stops when the source is a File-mode fresh create, and the attach
  step continues when the harness event arrives. ([#1596], [#1597])
- The contrast of the accent color in light mode. ([#1604])
- The Nix dev shell builds jemalloc again. ([#1614])

## [0.10.0] - 2026-10-01

The first tagged release. See the
[release notes](https://github.com/cortexapps/engrams/releases/tag/v0.10.0).

[Unreleased]: https://github.com/cortexapps/engrams/compare/v0.13.0...HEAD
[0.13.0]: https://github.com/cortexapps/engrams/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/cortexapps/engrams/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/cortexapps/engrams/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/cortexapps/engrams/releases/tag/v0.10.0
[#1471]: https://github.com/cortexapps/engrams/pull/1471
[#1568]: https://github.com/cortexapps/engrams/pull/1568
[#1573]: https://github.com/cortexapps/engrams/pull/1573
[#1574]: https://github.com/cortexapps/engrams/pull/1574
[#1575]: https://github.com/cortexapps/engrams/pull/1575
[#1576]: https://github.com/cortexapps/engrams/pull/1576
[#1577]: https://github.com/cortexapps/engrams/pull/1577
[#1578]: https://github.com/cortexapps/engrams/pull/1578
[#1579]: https://github.com/cortexapps/engrams/pull/1579
[#1581]: https://github.com/cortexapps/engrams/pull/1581
[#1582]: https://github.com/cortexapps/engrams/pull/1582
[#1583]: https://github.com/cortexapps/engrams/pull/1583
[#1584]: https://github.com/cortexapps/engrams/pull/1584
[#1585]: https://github.com/cortexapps/engrams/pull/1585
[#1586]: https://github.com/cortexapps/engrams/pull/1586
[#1587]: https://github.com/cortexapps/engrams/pull/1587
[#1588]: https://github.com/cortexapps/engrams/pull/1588
[#1589]: https://github.com/cortexapps/engrams/pull/1589
[#1590]: https://github.com/cortexapps/engrams/pull/1590
[#1591]: https://github.com/cortexapps/engrams/pull/1591
[#1596]: https://github.com/cortexapps/engrams/pull/1596
[#1597]: https://github.com/cortexapps/engrams/pull/1597
[#1599]: https://github.com/cortexapps/engrams/pull/1599
[#1601]: https://github.com/cortexapps/engrams/pull/1601
[#1602]: https://github.com/cortexapps/engrams/pull/1602
[#1603]: https://github.com/cortexapps/engrams/pull/1603
[#1604]: https://github.com/cortexapps/engrams/pull/1604
[#1605]: https://github.com/cortexapps/engrams/pull/1605
[#1607]: https://github.com/cortexapps/engrams/pull/1607
[#1608]: https://github.com/cortexapps/engrams/pull/1608
[#1610]: https://github.com/cortexapps/engrams/pull/1610
[#1611]: https://github.com/cortexapps/engrams/pull/1611
[#1612]: https://github.com/cortexapps/engrams/pull/1612
[#1613]: https://github.com/cortexapps/engrams/pull/1613
[#1614]: https://github.com/cortexapps/engrams/pull/1614
[#1617]: https://github.com/cortexapps/engrams/pull/1617
[#1619]: https://github.com/cortexapps/engrams/pull/1619
[#1642]: https://github.com/cortexapps/engrams/pull/1642
[#1644]: https://github.com/cortexapps/engrams/pull/1644
[#1645]: https://github.com/cortexapps/engrams/pull/1645
[#1652]: https://github.com/cortexapps/engrams/pull/1652
[#1654]: https://github.com/cortexapps/engrams/pull/1654
[#1658]: https://github.com/cortexapps/engrams/pull/1658
[#1660]: https://github.com/cortexapps/engrams/pull/1660
[#1661]: https://github.com/cortexapps/engrams/pull/1661
[#1662]: https://github.com/cortexapps/engrams/pull/1662
[#1663]: https://github.com/cortexapps/engrams/pull/1663
[#1664]: https://github.com/cortexapps/engrams/pull/1664
[#1670]: https://github.com/cortexapps/engrams/pull/1670
[#1671]: https://github.com/cortexapps/engrams/pull/1671
[#1672]: https://github.com/cortexapps/engrams/pull/1672
[#1675]: https://github.com/cortexapps/engrams/pull/1675
[#1693]: https://github.com/cortexapps/engrams/pull/1693
[#1694]: https://github.com/cortexapps/engrams/pull/1694
[#1695]: https://github.com/cortexapps/engrams/pull/1695
[#1700]: https://github.com/cortexapps/engrams/pull/1700
[#1702]: https://github.com/cortexapps/engrams/pull/1702
[#1703]: https://github.com/cortexapps/engrams/pull/1703
[#1709]: https://github.com/cortexapps/engrams/pull/1709
[#1711]: https://github.com/cortexapps/engrams/pull/1711
[#1713]: https://github.com/cortexapps/engrams/pull/1713
[#1714]: https://github.com/cortexapps/engrams/pull/1714
[#1716]: https://github.com/cortexapps/engrams/pull/1716
[#1717]: https://github.com/cortexapps/engrams/pull/1717
[#1719]: https://github.com/cortexapps/engrams/pull/1719
[#1728]: https://github.com/cortexapps/engrams/pull/1728
[#1730]: https://github.com/cortexapps/engrams/pull/1730
[#1733]: https://github.com/cortexapps/engrams/pull/1733
[#1734]: https://github.com/cortexapps/engrams/pull/1734
[#1736]: https://github.com/cortexapps/engrams/pull/1736
[#1744]: https://github.com/cortexapps/engrams/pull/1744
[#1745]: https://github.com/cortexapps/engrams/pull/1745
[#1750]: https://github.com/cortexapps/engrams/pull/1750
[#1753]: https://github.com/cortexapps/engrams/pull/1753
[#1755]: https://github.com/cortexapps/engrams/pull/1755
[#1758]: https://github.com/cortexapps/engrams/pull/1758
[#1759]: https://github.com/cortexapps/engrams/pull/1759
[#1764]: https://github.com/cortexapps/engrams/pull/1764
[#1766]: https://github.com/cortexapps/engrams/pull/1766
[#1778]: https://github.com/cortexapps/engrams/pull/1778
[#1782]: https://github.com/cortexapps/engrams/pull/1782
[#1783]: https://github.com/cortexapps/engrams/pull/1783
[#1784]: https://github.com/cortexapps/engrams/pull/1784
[#1788]: https://github.com/cortexapps/engrams/pull/1788
[#1789]: https://github.com/cortexapps/engrams/pull/1789
