---
title: FAQ
description: Short answers to the questions people ask before they run engrams.
sidebar:
  order: 4
---

## Is it ready for production?

It runs one company's production, on GKE, every day. It is also a 0.x project: the wire
formats between components change, the API is not versioned yet, and the Firecracker it ships
is a fork that is rebased on upstream daily. Plan on upgrading the whole deployment at once,
control plane and host fleet together.

## Do I need Firecracker?

For production, yes. Production isolation is Firecracker on Linux with KVM, and there is no
container mode. Two other backends exist for development: Apple's Virtualization framework on
an Apple Silicon Mac, which gives you real microVM isolation on a laptop, and a subprocess
backend with no isolation at all for iterating on the orchestration layer.

## Which hosts can run the fleet?

Any Linux node with KVM. On managed Kubernetes that means a node pool with nested
virtualization, which is Intel-only on every provider today: the C3 family on GKE, and either
m8i or bare metal on EKS. Bare metal anywhere works too; the host agent does not care who owns
the machine.

## Can I use my own images?

Yes. A session image is any Linux image with `/bin/sh`, built with `docker build` and pushed to
any registry. There is no build tool to learn and nothing of engrams inside the image. Use a
glibc-based image: the bundled agent binaries do not run on Alpine.

## Does anything happen without a person typing a prompt?

Yes. Automations start sessions from a schedule, a Slack mention, a pull request event, a
Linear issue, or a signed webhook, and can prompt them, wait for them, run commands inside
them, and post results to Slack or GitHub. Pull request review and the Slack thread brain
ship as automations you enable. There is no general approval block yet; a person is in the
loop through the Slack thread, the dashboard, and the pull request review itself.

## Which agents run inside?

Claude Code and Codex ship as harnesses. A harness is a bundle the host mounts into the VM at
boot, so the agent runtime is upgraded fleet-wide without touching images.

## Which models?

Whatever the harness supports with your key. The Claude Code harness talks to Anthropic with an
API key or a personal Claude Code token; the Codex harness talks to OpenAI with an API key or a
ChatGPT connection. Either can be pointed at OpenRouter instead, which fronts many providers
behind one key.

## How much does an idle session cost?

Nothing on the host. An idle session is a set of chunks in the blob store, deduplicated
against the image's base snapshot and every other session of that image, and a row in Postgres.
A thousand sessions of a 4 GiB image take about 100 GiB of blob storage, not 4 TiB.

## How fast is resume?

Under 100 ms when the session resumes on the host that snapshotted it and the chunks are still
in that host's cache. One to two seconds on a different host, which is also the cost of
migrating a session. A cold start from an image's base snapshot is under a second and needs no
pre-warmed pool. These are the numbers from the reference deployment; measure your own fleet
before you promise them to anyone.

## What is the license?

engrams is free software under the GNU Affero General Public License v3.0.
