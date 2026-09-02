# Contributing to engrams

Thank you for helping. This page is short on purpose. The build, the test lanes, and the
conventions live in [`AGENTS.md`](AGENTS.md); read it before your first change.

## Before your first pull request

- **Sign the CLA.** A bot asks for one comment on your first pull request. Read
  [`CLA.md`](CLA.md) first. You keep ownership of your work.
- **Open an issue for anything larger than a fix.** A short description of the problem and the
  shape of the change saves a rewrite after review.

## The pull request

- One logical change per pull request. Title in the form `scope: lowercase summary`.
- Body with `## Problem`, `## Fix`, and `## Test` sections.
- Run the smallest gate that covers your change (see "Change-scoped validation" in
  `AGENTS.md`). For Rust changes that is `just check` once before you push.
- Write in plain, direct English (we follow ASD-STE100, Simplified Technical English).

## Licenses

engrams is AGPL-3.0-only. The harness SDK crates (`engram-harness-sdk`,
`engram-harness-proto`, `engram-transport`, `engram-ids`) are Apache-2.0 so that a custom
harness can link them without taking on AGPL terms. A contribution to a crate goes out under
that crate's license.

## Security

Do not open a public issue for a vulnerability. See [`SECURITY.md`](SECURITY.md).
