# ADR 0117: First-class model routers

Status: Accepted

Date: 2026-08-13

## Context

Harness descriptors currently own model routing. The Claude descriptor includes
OpenRouter models, OpenRouter credentials, and provider egress. This makes a
router available to one harness only. It also lets a model option change the
credential boundary of a launch.

A model router and an agent harness are independent choices. A router supplies
a model API, a catalog, policy, credentials, and egress. A harness supplies the
agent runtime and one or more protocol adapters.

## Decision

A launch selects a harness, an optional model router, a model, and an effort.
An absent router selects the harness's native provider. The orchestrator owns a
registry of model routers and their protocol endpoints. The first built-in
router is `openrouter`.

Harness descriptors declare the router protocols that they can consume. They
also separate always-needed egress from native-provider egress. Model and effort
options contain configuration only. They cannot declare secrets.

The orchestrator owns the routed model catalog and policy. It synchronizes the
OpenRouter catalog from the authenticated `/models/user` endpoint. A refresh is
transactional. A failed or truncated response leaves the previous complete
cache in place. A successful refresh marks unseen rows unavailable without
deleting policy.

Policy has two gates. `enabled` permits service-account and ownerless automation
launches. Human-owned launches also require `user_enabled`. The database
enforces that `user_enabled` implies `enabled`.

Routed launches use the router's sealed organization credential and egress.
They do not use native harness credentials or native-provider egress. The
orchestrator passes one normalized `ENGRAM_MODEL_ROUTER_*` contract to the
harness. Built-in Claude and Codex adapters translate that contract into their
native provider configuration. Custom harnesses can consume the contract after
they declare a compatible protocol.

OpenRouter catalog membership is capability-based. We include non-batch models
that declare text output and tool support. We do not keep an open-weight
allowlist. Newly discovered models start disabled.

## Consequences

- A routed model works with every harness that supports its protocol.
- Router credentials and integration grants have separate authority.
- A missing key, unavailable model, policy rejection, or protocol mismatch
  fails before VM creation.
- The router registry is reusable, but version 1 registers only OpenRouter.
- Existing running sessions keep their compiled environment.
- New router support requires an orchestrator registry entry and a harness
  protocol adapter. It does not require model options in a harness descriptor.

## Migration

The migration moves `glm-5.2` to `openrouter/z-ai/glm-5.2` and
`deepseek-v4-flash` to
`openrouter/deepseek/deepseek-v4-flash-0731` in profiles, task echoes, recursive
launch snapshots, and automation actions. It seeds both models and
`deepseek/deepseek-v4-pro-0813` as enabled and available to users.

The Claude descriptor removes its two OpenRouter model options. There is no
descriptor compatibility shim.

## Decision history

- Proposed on 2026-08-13 before implementation.
- Accepted on 2026-08-13 after the protocol, persistence, launch adapters,
  administration surface, migration, and tests were completed.
