# Building session images

An engrams session image is a **plain OCI image** — `docker build && docker push` to any registry. There's no engram-specific build tool, no `engram.toml`, no local ext4 bake, and nothing engrams-owned baked into the rootfs (agentd, the harness, and ttyd all ride host-staged bundle slots, swapped in per-session). The image contract is just "any linux image with `/bin/sh`". `git` / `curl` / `socat` / `iproute2` are **workspace** requirements — install them in your Dockerfile if your agent needs them (the demo image does) — not engrams requirements.

```
my-repo/
├── Dockerfile
└── ... your code
```

Build and push it like any other container image:

```bash
docker build -t localhost:5001/cortex/api:warm-1 .
docker push  localhost:5001/cortex/api:warm-1
# or, equivalently, the dev helper:
TAG=warm-1 just bake cortex/api ./path/to/repo
```

Runtime config — name, description, env, workdir, resources, and the warm-capture hook with its env and egress — is entered **at enable time** in the dashboard (Operator → Images → Enable a new image) and stored by the coordinator. It is *not* in the image. The CLI covers the fields a script needs:

```bash
engrams image enable --uri localhost:5001/cortex/api:warm-1 --name cortex-api --vcpus 2 --memory-mib 4096
# edit later (cheap fields apply immediately; resources need --allow-recapture):
engrams image update --uri localhost:5001/cortex/api:warm-1 --memory-mib 8192 --allow-recapture
engrams image config --uri localhost:5001/cortex/api:warm-1   # the stored config, as JSON

SID=$(engrams session create --image localhost:5001/cortex/api:warm-1)
```

The warm hook (command, timeout, capture env, capture network) is set in the dashboard.

At enable (and on rebase) the coordinator materializes the OCI image into a chunked ext4 rootfs **host-side** via the `MaterializeImage` host RPC (`engram-rootfs-materializer`, pull-pipelined whiteout-aware declare → seal a deterministic pure-Rust ext4 layout (`mkext4`) with the stage-1 `/sbin/engram-init` shim declared in → stream-fill straight into chunked `BlobStorage` — no tree, no image file). That init shim is the only engrams-owned file baked into the rootfs; per-session latency is zero (materialize is enable/rebase-time only). Harness-level credentials (`CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`, …) live one layer above the image and are handled per-harness at session-create time — see `DESIGN.md` for the full image / config / secret model.

Building + pushing the image requires Docker (Docker Desktop, OrbStack, Colima, or Podman with the docker-compat shim).
