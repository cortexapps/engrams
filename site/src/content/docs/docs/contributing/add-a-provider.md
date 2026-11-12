---
title: Add a provider
description: How a git forge, a chat service, or a model router plugs into engrams, and what each one has to implement.
sidebar:
  order: 2
---

engrams reaches outside services through three roles. A provider that fills a role gets
the product features built on it, with no change to the features themselves.

| Role | What it gets | Ships today |
|---|---|---|
| Git forge | Clone and push from a session, the pull request review built-in, pull request links on tasks | GitHub, GitLab |
| Chat | The thread bot built-in, replies and questions in a thread, choice buttons | Slack |
| Model router | Routed launches for every harness, task titles, the Decide block | OpenRouter, LiteLLM |

A provider is two things in `orchestrator/`:

- **A connector**, `src/connectors/<name>.json`. It declares the hosts, the credential and
  how it is added to a request, the settings an admin fills in, the operations a session may
  be granted, the webhook events, and the actions an automation can call.
- **A module**, `src/providers/<name>/`. It holds every line of code that knows the
  provider. Code outside this directory does not name a provider; a test fails when it does.

The role's registry lists the provider's spec, and that one line is the only edit outside
the provider's own files.

## A connector alone

A connector with no module already gives sessions brokered access: a profile grants its
operations, and the egress proxy adds the credential to each allowed request. The agent
never sees the credential. Start here; many services need nothing more.

A service that sends webhooks declares them in the connector's `webhook` section and adds
`src/providers/<name>/events.ts`, which says what a verified delivery is: its event key, its
delivery id, and the fields automations can filter on. Every provider's deliveries arrive at
`/api/v1/integrations/<name>/events`. The connector's ingress scheme selects how a delivery
is verified. If the service signs or authenticates deliveries in a way no scheme covers, add
a scheme to `src/automations/verify.ts`.

## A model router

Implement `ModelRouterSpec` (`src/model-routers/registry.ts`):

- the protocols the router serves (Anthropic Messages, OpenAI Responses) and the base path
  of each;
- the path of its model list and a parser for it;
- a chat-model factory for the calls the orchestrator makes itself (titles, decisions);
- optionally a built-in model for each role, a public page for a model, and a native
  decisions endpoint.

A router with no native decisions endpoint answers a Decide block with a structured chat
completion. A router with no built-in model for a role shows a picker to the admin.
`src/providers/litellm/router.ts` is a complete example in about 70 lines.

## A git forge

Git access comes first and needs no module. A forge that mints short-lived tokens answers
git's credential helper in the session. A forge with a static token declares the git paths
(`info/refs`, `git-upload-pack`, `git-receive-pack`) as operations in its connector, and the
egress proxy adds the token, so the token never enters the session.

For the pull request review, implement `GitForge` (`src/forges/forge.ts`): read a pull
request, list its review comments, post a review, find a posted review by its marker, parse
a pull request URL, and build the links to a pull request, a review, a comment, and a file.
Then add a review variant (`src/automations/builtins/review-variant.ts`): the events that
start and close a review, the checkout command, and the capabilities a reviewer session
needs. The review's graph is shared; the variant fills its provider-shaped slots.

A forge that can check a commenter's permission and edit a comment declares `typedReview`,
and its variant uses the built-in `review_facts` step. `src/providers/gitlab/` is the
example to copy.

## A chat provider

Implement `ChatProvider` (`src/chat/chat.ts`): how the agent talks in a thread (post,
update, ask, offer choices), a user's email from their id, the channel list for the
enrollment picker, and the handle of a thread. Add a threads variant
(`src/automations/builtins/threads-variant.ts`) with the events that start and continue a
thread. A provider with buttons or cards also serves an interactivity route.

Slack is the only chat provider today, so this seam has had one implementation. Expect to
find a Slack-shaped corner when you add the second, and change the seam where you do.

## What is not there yet

- **Azure DevOps** has a connector (git over HTTPS, pull requests, work items, pipelines)
  and no review: a service hook is not signed, so it needs its own ingress scheme, and then
  a `GitForge`.
- **Microsoft Teams** has no connector and no chat provider.
- **One connection per provider.** An organization can use GitHub and GitLab together, but
  not two GitLab hosts.

## Tests

Each role has tests to extend, and all of them run under `bun test` in `orchestrator/`:

- A forge: the GitLab suite in `src/providers/gitlab/__tests__/` shows the cases: event
  classification, admission, read, post, and a post that resumes after a failure.
- A built-in variant: the definition must validate, and one scenario suite drives each
  variant through the interpreter.
- The definitions of the GitHub review and the Slack thread bot are pinned by hash. A change
  to a shared builder that moves either hash fails a test, because a new definition stops
  the active runs of that built-in on every deployment.
