# Changelog

This file records the notable changes in each release of engrams. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the version numbers follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

engrams is a 0.x project, so a minor release can contain breaking changes. Each release
starts with **Upgrade notes**: read them before you upgrade. The upgrade procedure is in
[Releases and upgrades](docs/deploy.md#releases-and-upgrades).

## [Unreleased]

### Upgrade notes

- **The `engrams` CLI is a Rust binary.** The release assets keep their names
  (`engrams-linux-x64`, `engrams-linux-arm64`, `engrams-darwin-arm64`), and the commands,
  flags, `--json` shapes, `hosts.json` credential file, and exit codes do not change. Build it
  from the repository with `cargo build --profile cli -p engram-cli`; the `cli/` directory and
  its Bun toolchain are gone.

### Changed

- The `engrams` CLI moved from Bun to Rust (`crates/engram-cli` on the new
  `crates/engram-app-client` library). The binary is 3 MB and uses about 8 MB of memory.

### Fixed

- `engrams session logs` printed the event-stream envelope instead of the event payload.

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

[Unreleased]: https://github.com/cortexapps/engrams/compare/v0.11.0...HEAD
[0.11.0]: https://github.com/cortexapps/engrams/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/cortexapps/engrams/releases/tag/v0.10.0
[#1471]: https://github.com/cortexapps/engrams/pull/1471
[#1568]: https://github.com/cortexapps/engrams/pull/1568
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
