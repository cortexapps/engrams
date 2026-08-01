# syntax=docker/dockerfile:1.7
FROM rust:1.95-slim AS builder
WORKDIR /src
# protobuf-compiler: ADR 0013 added a build.rs in engram-protocol that
# invokes `protoc` to compile `proto/host_service.proto`. The Debian
# package version is compatible with `tonic-build` 0.12.
# clang + libclang-dev: ADR 0068 added a Linux-gated `userfaultfd` dep to
# engram-host-agent (the uffd_minor_shmem capability probe), which the
# coordinator inherits via its RunMode::All/LocalHostClient dependency;
# userfaultfd-sys's build.rs runs bindgen against <linux/userfaultfd.h>,
# and bindgen needs libclang. Same pair host-agent.Dockerfile installs
# for the same crate (ADR 0044 K2).
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates curl protobuf-compiler clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*
COPY . .
ARG TARGETARCH
RUN case "$TARGETARCH" in amd64) codex_arch=x86_64 ;; arm64) codex_arch=aarch64 ;; \
      *) echo "unsupported Codex architecture: $TARGETARCH" >&2; exit 1 ;; esac \
    && deploy/harness-codex/fetch-codex.sh "$codex_arch" /tmp/codex-package \
    && cp /tmp/codex-package/bin/codex /tmp/codex
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p engram-coordinator \
    && cp target/release/engram-coordinator /tmp/engram-coordinator

# Trixie matches the glibc version `rust:1.95-slim` builds against
# (currently 2.39). bookworm (2.36) would refuse to load the binary
# with: `version GLIBC_2.39 not found`.
FROM debian:trixie-slim
# squashfs-tools: ADR 0055 P2 — the coordinator's `skill_pack` shells to
# `mksquashfs` to pack an uploaded skill dir into a content-addressed RO
# squashfs at registration (MountCatalogService.RegisterSkill).
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates squashfs-tools \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /tmp/engram-coordinator /usr/local/bin/engram-coordinator
COPY --from=builder /tmp/codex /usr/local/bin/codex
COPY deploy/migrations /opt/engram/migrations
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/engram-coordinator"]
