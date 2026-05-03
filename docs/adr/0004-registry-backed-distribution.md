# ADR 0004: Registry-backed image and harness distribution

Status: accepted, 2026-05-03
Phase: 5

## Context

Phase 4 served bake images and harness packs from the coordinator's
local disk:

- Bake images at `<local_path>/images/<repo>/<tag>/{manifest.toml,
  rootfs.ext4}`. Coordinator scanned this tree for the dashboard and
  resolved `(repo, tag)` to a filesystem path at session-create time.
- Harness packs at `<harnesses_dir>/<name>/{harness, sidecars...}`.
  Scanned once at startup and packed into a single substrate ext4
  mounted into every sandbox.

This was the right shape for a single-coordinator dev story but
breaks down for production:

1. **HA blocks redeploys.** Every coordinator replica needs the full
   image set on disk. New images can't appear without a coordinator
   redeploy or a shared filesystem nobody wants to operate.
2. **Disk is the binding constraint.** A modest org with 10 images at
   2 GiB each is 20 GiB on every replica. We're funding redundant
   capacity that scales with image count, not request volume.
3. **No dynamic add.** Users can't push a new starter image or
   harness without dropping bytes onto every coordinator host. CD
   pipelines exist for exactly this; we shouldn't reinvent them.

ADR 0001 noted the direction in passing: "Image distribution
(Phase 5+) will use a Docker registry — more standard, plays with
existing tooling." This ADR is the concrete design.

## Decision

**Both bake images and harness packs move to a Docker registry.**
Coordinator becomes stateless w.r.t. image and harness data — it
holds only Postgres pointers (registry URIs). Host-agents pull on
first use and cache content-addressably on local NVMe. Registry
credentials live in Postgres, envelope-encrypted under a deployment
KEK.

### Artifact format

We do **not** push the source OCI image (the `docker build` output).
We push the *pre-baked* rootfs as a custom OCI artifact. This means:

- Production hosts don't need Docker installed — they just need an
  HTTP client that speaks OCI Distribution Spec.
- Cold pull is one blob fetch + one filesystem write. No `docker
  pull → docker create → docker export → mke2fs` work duplicated on
  every host.
- The artifact is content-addressable: the registry digest *is* the
  exact bytes the VM boots, so two hosts pulling the same tag at the
  same time provably attach the same root drive.

Trade-off: you can't `docker run -it gcr.io/cortex/api:warm-X bash`
to introspect the image. Engram-specific tooling (the dashboard,
`engram image list`) is the inspection path. For external registry
UIs, the artifact appears with a custom mediaType — recognizable as
"some Engram artifact" but not directly viewable.

Bake image artifact:

```
config:    application/vnd.engram.image.v1+json    (small JSON)
layers[0]: application/vnd.engram.manifest.v1+toml (the manifest.toml)
layers[1]: application/vnd.engram.rootfs.ext4.v1   (the rootfs.ext4)
```

Harness pack artifact:

```
config:    application/vnd.engram.harness.v1+json (small JSON)
layers[0]: application/vnd.engram.harness.tar.v1+gzip (gzip'd tar of the pack dir)
```

Standard registries (GHCR, GCR, ECR, Harbor, `registry:2`) accept
arbitrary mediaTypes since the OCI Distribution Spec was updated in
2022, so this works against every registry users would actually
deploy against.

### Encryption: KEK → DEK envelope

Per-credential AES-256-GCM data key (DEK), wrapped by a deployment
master key (KEK). The KEK lives outside Postgres; only wrapped DEKs
land in the database. Two providers:

- **`EnvVarKeyProvider`** — reads a 32-byte base64-encoded master
  key from a process env var (default `ENGRAM_KEK_MASTER_KEY`). Used
  for dev (`just bootstrap` writes it to `.env`) and for prod
  deployments that source secrets from k8s Secrets / Doppler /
  Vault / GCP Secret Manager into env. Fails closed at startup if
  the var is missing or not 32 bytes.
- **`GcpKmsProvider`** — defers wrap/unwrap to GCP KMS so the KEK
  never leaves KMS at all. Stub today; wired when needed.

Why envelope (and not direct KMS encrypt of the password):

1. **Key rotation without re-encrypting rows.** Rotate the KEK,
   re-wrap the row's DEK lazily on next read.
2. **Bounded KMS calls.** One wrap per write, one unwrap per read
   regardless of plaintext size.
3. **Generalizes.** Same shape works when we later store SSH keys,
   JWT signers, OAuth refresh tokens. Nothing here is registry-
   specific except the call site.

### Storage: dual paths, eventually one

`image_versions.blob_url` (already nullable from Phase 4) gets a
new meaning: when set, it's an OCI URI; when NULL, the legacy
on-disk path applies. This lets the change land incrementally —
deploy the new code, push the first image, drop the disk path when
all rows are URIs.

`harness_packs` is a new table; when empty, the legacy
`HarnessRegistry::from_dir` scan applies.

`registry_credentials` is the new table holding the envelope-
encrypted credentials. Lookup is by `registry_host`; missing rows =
anonymous pull (works for public registries and `localhost:5000`).

### Local dev

`deploy/docker-compose.yml` runs a `registry:2` service alongside
Postgres. `just dev{-firecracker,-vz}` brings it up automatically.
The OCI client allows plaintext HTTP only when the registry host is
`localhost` / `127.0.0.1` / `::1` (matching Docker's
`--insecure-registry` heuristic) — safe-by-construction; no flag to
forget.

The dev URL is `localhost:5000/cortex/api:warm-X`, no auth needed.
Anywhere else in the world (`gcr.io`, `ghcr.io`, ...) the client
forces HTTPS and looks up creds in Postgres.

## What this does *not* solve

- **Short-lived registry tokens** (ECR `GetAuthorizationToken`, GCP
  Workload Identity). v1 stores long-lived creds; ECR users rotate
  the stored password via cron until a credential-helper trait
  lands.
- **Multi-arch images.** v1 pushes single-arch artifacts. Multi-arch
  goes through the OCI Image Index; lands when a real cross-arch
  use case shows up.
- **DEK in-memory cache.** v1 unwraps every read. Add caching when
  KMS round trips become a measurable cost.
- **Image scanning / vulnerability gates.** Pre-baked artifacts
  aren't readable by Trivy/Grype anyway; that's its own design.

## Consequences

**Stateless coordinator becomes real.** Phase 4 already had every
durable bit in Postgres + git; the on-disk image and harness trees
were the last load-bearing local state. Removing them lets us
deploy the coordinator like any other stateless web service —
rolling redeploys, autoscaling on request rate, no shared volumes.

**Host-agent gains a content-addressable cache.** New responsibility
on the host side: pull from registry, cache by digest, GC under a
disk budget. The cache is keyed by the manifest digest the
registry returns, not by `(repo, tag)`, so re-pulls of an unchanged
tag are free and concurrent pulls of the same digest from different
sessions deduplicate naturally.

**Users get a `docker push`-shaped story.** `engram image build
--push localhost:5000/cortex/api` Just Works for dev;
`engram image build --push gcr.io/cortex/api` works for prod once
`engram registry add --host gcr.io ...` is run once per
deployment. Anyone who's used Docker has the muscle memory.

**Larger blast radius for KEK loss.** All registry passwords land
in Postgres encrypted under the KEK. If the KEK is lost (unbacked
env var, deleted KMS key) every credential row becomes unreadable
and has to be re-added by hand. This is the same trade-off RDS /
Cloud SQL / Vault all make; the answer is the same: back up the
KEK separately from the database, ideally in a KMS that has its own
durability story.

## Implementation tracks

Track 0: Foundation — `engram-crypto` crate (envelope encryption,
KEK providers), `engram-oci` crate (OCI Distribution client), new
Postgres tables + store methods.

Track A: Coordinator surface — `/api/registries` and
`/api/harnesses` CRUD, KEK wired into `Services`, image-builder
`--push` flag.

Track B: CLI — `engram registry add/list/rm`, `engram harness
add/list/rm`, `engram image build --push`.

Track C: Local dev — `registry:2` in docker-compose, `just
bootstrap` for KEK generation, `just dev-{vz,firecracker}` brings
the registry up automatically.

Track D: Host-agent pull path — content-addressable cache, OCI
client wired into `SandboxBackend::create`, per-session single-
harness substrate ext4 build (mke2fs from a `<name>/` symlink
staging tree). Coordinator falls back to legacy on-disk paths
when `image_versions.blob_url` is NULL or `harness_packs` has no
matching row.

Track E (5b): Polymorphic auth — `RegistryAuthSpec` enum dispatches
between static (envelope-encrypted in Postgres) and cloud-IAM
kinds (today: GCP Workload Identity via the `gcp_auth` crate).
AWS instance role / cross-account assume-role / GCP impersonation
slot in as new variants without schema migration. New crate
`engram-oci-auth` composes engram-oci/engram-core/engram-crypto
into a single `PgAuthResolver` that the coordinator wires into the
OCI client. ADR 0004 amended to reflect the SaaS-shape decision:
"every registry has the same credential shape" was the wrong
framing; we now model registry auth as variant-discriminated and
let the variant decide whether stored secret material exists.
