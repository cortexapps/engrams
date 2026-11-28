# Fixed platform image recipes

These recipes supply the `guard` and `fixture` image inputs for the platform
adapter. They build from the repository root for `linux/amd64` or `linux/arm64`.
They do not build a worker image or install packages in a guest.

The image producer must use a clean checkout and pass its full current HEAD as
`SOURCE_COMMIT`. Both recipes reject a missing or malformed value and set
`org.opencontainers.image.revision`. The fixture preflight checks that label
against `Inputs.sourceCommit`. The producer must still export a single-platform
OCI archive, record its file hash, manifest digest and config digest, and load
the same image into the selected engine. A recipe is not a physical test result.

## Bases

| Recipe | Official immutable image index |
| --- | --- |
| Guard | `debian:trixie-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4` |
| Fixture | `node:24.22.2-bookworm-slim@sha256:c86ecead9e55a5210ac1727de38a4ca840bf03dcaf34dc9749addbf383b1920c` |

Both [official Debian](https://hub.docker.com/_/debian) and
[official Node](https://hub.docker.com/_/node) indexes were read from the Docker
Registry on 2026-11-02. Each index has Linux amd64 and arm64 manifests. Guard
packages come from the fixed Debian and Debian Security snapshots at
`20261102T000000Z`. APT checks repository signatures; the fixed snapshots disable
only metadata expiry checks. No runtime package installation occurs.

## Guard consumers

| Consumer | Required paths and behavior |
| --- | --- |
| `engine.ts` projection | `/bin/sleep`, `/bin/sh`, `/bin/mkdir`, `/bin/chown`, `/bin/chmod`, `/usr/bin/find`, `/usr/bin/sha256sum` |
| `engine.ts` network guards | `/usr/sbin/nft`, `/usr/bin/nc`, `/usr/bin/curl`, `/usr/bin/dig` |
| `assets.ts` archive checks and guest publication | `/bin/sleep`, `/usr/bin/skopeo` |
| `assets.ts` bundle packing | `/bin/bash`, `/usr/bin/mksquashfs`; coreutils supply `dirname`, `rm`, `cut`, and `sha256sum` for the production harness leaves and `_pack.sh` |
| `launch.ts` dependency probes and denied endpoint | `/bin/sleep`, `/usr/bin/nc`, `/usr/bin/dig`, `/usr/bin/curl` |

The default user is `1000:1000`. The launcher overrides it with the caller's
numeric UID:GID for archive checks, publication, and packing. `HOME=/tmp` permits
these operations without a passwd entry. Packing uses the writable staging bind
at its original absolute path, read-only production recipes at `/recipes`, and
`/tmp` tmpfs. Archive inputs are read-only files under `/input`. The image carries
no engine client, engine socket, host credentials, auth file, or proxy setting.
Skopeo uses its packaged system policy and the explicit archive or registry
arguments from the adapter.

The engine drops all capabilities. The launcher grants `CHOWN`, `DAC_OVERRIDE`,
and `FOWNER` only to the root file-projection container. Network guards run as
root with `NET_ADMIN` only, a read-only root filesystem, `/tmp` tmpfs, and a
read-only CA-only projection at `/etc/mcp-probe`. The recipe grants no capability.
The denied endpoint and dependency probes run as `1000:1000`.

## Fixture consumers

`launch.ts` uses this exact command for both fixture roles:

```text
/usr/local/bin/node /app/tests/fixtures/mcp-stack/main.mts CONFIG
```

`CONFIG` is the absolute path to the launcher's private config. It selects the
modern or legacy protocol. There is no separate protocol command. The launcher
overrides the default `node` user with its current numeric UID:GID, mounts the
private run root writable at the same absolute path, and makes the root
filesystem read-only. `/tmp` is a tmpfs. The private run root contains the config,
dynamic TLS files, neutral API and control credentials, and durable effect state.
It must be accessible to that UID:GID. The image does not contain this data.

The image has the unchanged `main.mts`, `config.mts`, and `durable.mts` modules at
`/app/tests/fixtures/mcp-stack/`. It also has the unchanged `server.ts`, `http.ts`,
and `oauth.ts` modules at `/app/mcp-worker/test/fixtures/`, and
`/app/mcp-worker/package.json` to retain the module scope. Runtime imports use
Node built-ins and these local files. The only package import is the type-only
`Tool` import from `@modelcontextprotocol/client`; Node 24 erases it. No npm
runtime dependency or install is needed for this entrypoint.

The recipe generates `tls.ts` with empty-string `TEST_CA`, `TEST_CERT`, and
`TEST_KEY` exports. It never copies the source TLS module into an image stage.
The final image copies only the prepared module closure. No image layer contains
the static test TLS key. `main.mts` requires a config, validates the runtime TLS
and authority fields, and passes them to the fixture. Missing runtime TLS fails
startup. The empty exports provide no valid fallback TLS identity or trust root.

The existing literal `fixture-client-secret` in `oauth.ts` is a public,
nonproduction protocol test datum. It remains unchanged so the deterministic
OAuth fixture keeps its existing behavior. Dynamic keys, tokens, credentials,
catalog configuration, and durable state remain launcher-owned.

## Validation boundary

Static source checks can establish paths, module closure, fixed base identities,
and recipe syntax. They do not establish package installation, final executable
behavior, arbitrary-user packing, TLS service startup, OCI archive linkage,
engine loading, or either physical backend result. Image build and physical
consumer checks must establish those facts before acceptance.

The guard also carries `/usr/sbin/ip` from `iproute2` for the fixed native-host
/32 route in its own network namespace, and `/usr/bin/openssl` for the short
coordinator-client mutual TLS probe. Both tools use the same fixed APT snapshot
and executable checks as the existing tools. These tools add no host route,
capability, mount or credential to the worker.
