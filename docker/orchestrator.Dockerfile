# syntax=docker/dockerfile:1.7
#
# The engrams orchestrator (ADR 0051) — a Bun + Hono service that fronts the
# Rust coordinator with per-user authz, a native task model, and the browser
# HTTP/WS surface. Entry is `bun src/index.ts` (package.json `start`), listening
# on ORCHESTRATOR_PORT (default 8787).
#
# Mirrors docker/web.Dockerfile's two-stage shape: a deps layer keyed on the
# lockfile (stays warm across source edits), then a slim runtime that ships only
# the production node_modules + the source. Built from the repo root:
#
#   docker buildx build -f docker/orchestrator.Dockerfile -t engram/orchestrator:dev .

# EXACT version, and it must equal `.bun-version` at the repo root — CI reads
# that file via setup-bun's `bun-version-file`, and a `just check` step fails
# the build if the two drift.
#
# This was `oven/bun:1`, a floating major. Every bake silently took the newest
# Bun 1.x while CI stayed pinned to 1.3.14, so the runtime prod ran was never
# the runtime the tests ran. Bun 1.4.0 then tightened the window on native
# `server.upgrade()`, which broke every WebSocket in the product — the spec
# document, the IDE, previews — with CI fully green, because on 1.3.14 the same
# code is fine. Bumping Bun is a decision that belongs in a reviewed diff.
FROM oven/bun:1.4.2-alpine AS deps
WORKDIR /app/orchestrator

# Lockfile + manifest first so the production install layer stays warm across
# source-only edits. --frozen-lockfile fails if bun.lock is stale (reproducible
# builds); --production drops devDependencies (drizzle-kit, tsc, @types) — the
# runtime runs the .ts entry directly via Bun, no build/transpile step.
COPY orchestrator/package.json orchestrator/bun.lock ./
COPY orchestrator/packages/spec-document ./packages/spec-document
RUN --mount=type=cache,target=/root/.bun/install/cache \
    bun install --frozen-lockfile --production

# ── runtime ──────────────────────────────────────────────────────────────
# Keep in lockstep with the deps stage and `.bun-version` (see above).
FROM oven/bun:1.4.2-alpine
WORKDIR /app/orchestrator

# The orchestrator runs the TypeScript entry directly under Bun (no compile
# step), so the runtime needs the production node_modules + the full source.
COPY --from=deps /app/orchestrator/node_modules ./node_modules
COPY orchestrator/ ./

# The Google credential denylist is ONE checked-in table shared with the Rust
# egress proxy, which is the enforcement point and owns the file (ADR 0109).
# `src/integrations/google-credential-denylist.ts` imports it by a repo-relative
# path, so the image has to keep that same relative layout.
COPY crates/engram-egress-proxy/policy/google-credential-denylist.json \
     /app/crates/engram-egress-proxy/policy/google-credential-denylist.json

# The oven/bun image ships a non-root `bun` user (UID 1000). Run as it; the
# chart hardens further (readOnlyRootFilesystem, drop ALL caps). Nothing here
# writes to the image FS at runtime.
USER bun

# Documents the default; the chart sets ORCHESTRATOR_PORT explicitly and maps
# the Service/healthcheck to it. Keep in sync with config.ts's 8787 default.
EXPOSE 8787

# `bun run start` === `bun src/index.ts` (orchestrator/package.json). The server
# binds 0.0.0.0:${ORCHESTRATOR_PORT} and refuses to boot without the three
# required env vars (ORCHESTRATOR_DATABASE_URL, CONTROL_PLANE_BEARER,
# BETTER_AUTH_SECRET) — see orchestrator/src/config.ts.
CMD ["bun", "run", "start"]
