# syntax=docker/dockerfile:1.7
FROM rust:1.95-slim AS builder
WORKDIR /src
# protobuf-compiler: ADR 0013 added a build.rs in engram-protocol that
# invokes `protoc` to compile `proto/host_service.proto`. The Debian
# package version is compatible with `tonic-build` 0.12.
# clang + libclang-dev: ADR 0044 K2 — engram-uffd-handler pulls in
# userfaultfd-sys, whose build.rs runs bindgen against <linux/userfaultfd.h>,
# and bindgen needs libclang. (Only needed in this builder stage; the slim
# runtime image below doesn't carry them.)
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates protobuf-compiler clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p engram-host-agent -p engram-uffd-handler \
    && cp target/release/engram-host-agent /tmp/engram-host-agent \
    && cp target/release/engram-uffd-handler /tmp/engram-uffd-handler

# Trixie matches the builder's glibc — bookworm (2.36) refuses
# binaries linked against trixie's glibc 2.39+.
FROM debian:trixie-slim
# Runtime deps the host-agent shells out to (on the GCE/Packer hosts
# these come from the node image; the K8s host-agent image must carry
# them itself — ADR 0044 K1):
#   - e2fsprogs: mke2fs, to materialize the per-sandbox ext4 rootfs
#   - iproute2:  ip, for the tap device + guest netns
#   - iptables:  egress NAT / firewall rules for guest networking
# (ca-certificates: TLS to the blob backend — GCS/S3.)
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates e2fsprogs iproute2 iptables \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /tmp/engram-host-agent /usr/local/bin/engram-host-agent
# ADR 0044 K2: the UFFD memory-restore handler. On the GCE/Packer hosts it
# came from the node image; the K8s host-agent image must carry it (a PATH
# lookup of `engram-uffd-handler` is the FirecrackerConfig default). Without
# it, every Uffd-mode restore/resume page-faults forever / fails to spawn.
COPY --from=builder /tmp/engram-uffd-handler /usr/local/bin/engram-uffd-handler
ENTRYPOINT ["/usr/local/bin/engram-host-agent"]
