---
title: Glossary
description: The words engrams uses, each with the page that owns it.
sidebar:
  order: 5
---

**Session.** One running sandbox: an image plus an optional harness in one microVM on one
host. Sessions are created, become active, go idle, resume, and end. See
[What is engrams](../overview/).

**Task.** One unit of agent work, started from a profile with a prompt. A task owns one or
more sessions and is what the dashboard, Slack, and `engrams task` show.

**Profile.** An admin-curated bundle of session settings: image, repositories, environment,
egress policy, integration access, skills, and session apps.

**Automation.** A trigger, a tree of blocks, and typed inputs that start and drive sessions
without a person typing a prompt. Runs are durable and recorded step by step. See
[Automations](../../platform/automations/).

**Model router.** A service such as OpenRouter that supplies models to a harness in place of
the provider's own API, with a per-model policy for who may launch them. See
[Model routers](../../guides/model-routers/).

**Session app.** An HTTP service inside a session, published at its own hostname behind the
login wall. See [Session apps](../../guides/session-apps/).

**Image.** A plain OCI image, built with `docker build` and pushed to any registry, that a
session boots from. See [Images and harnesses](../../concepts/images-and-harnesses/).

**Image config.** The runtime settings for an image that are not in the image: name,
environment, working directory, resources, and the warm hook. Entered in the dashboard when
the image is enabled. See [Images and harnesses](../../concepts/images-and-harnesses/).

**Enable.** The step that turns a pushed image into something sessions can boot: engrams
materializes the image into chunks, boots a capture VM on the fleet, runs the warm hook, and
freezes a base snapshot.

**Base snapshot.** The frozen memory and disk of an enabled image after its warm hook ran.
Every session of that image starts from it.

**Harness.** The agent runtime inside the VM, such as Claude Code or Codex. Staged by the host
as a read-only bundle and mounted at boot, never baked into the image.

**Host.** A Linux machine with KVM that runs microVMs. Hosts dial the coordinator and need no
inbound port.

**Backend.** The virtual machine technology a host uses: Firecracker in production, Apple
Virtualization on a Mac, or plain subprocesses. See
[Sandbox backends](../../concepts/sandbox-backends/).

**Snapshot.** A session's memory and disk, written as chunks so the VM can be destroyed and
restored later. Taken when a session goes idle.

**Chunk.** A fixed-size, immutable block of disk or memory, stored under the hash of its
content. Chunks are what make snapshots cheap and deduplicated. See
[Storage and snapshots](../../concepts/storage-and-snapshots/).

**Manifest.** A versioned list of the chunks that make up one disk or memory image. A session's
manifest points at the base image's chunks plus the ones the session changed.

**Warm hook.** A command run once inside the capture VM before the base snapshot is frozen, so
sessions start with caches filled and daemons running. See
[Authoring a warm hook](../../guides/warm-hooks/).

**Connection.** A configured, credentialed instance of an integration, such as a Slack
workspace or a Google Cloud service account, that profiles can grant to sessions.

**Skill.** A read-only bundle of tools mounted into the VM alongside the harness: shared
browser, IDE, git credentials, and admin-uploaded packs.

**API key.** A credential for the API and CLI. Personal keys come from `engrams auth login`;
admin service keys are minted in Settings for scripts and CI.
