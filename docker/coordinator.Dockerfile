# syntax=docker/dockerfile:1.7
FROM rust:1.96-slim AS builder
WORKDIR /src
# protobuf-compiler: ADR 0013 added a build.rs in engram-protocol that
# invokes `protoc` to compile `proto/host_service.proto`. The Debian
# package version is compatible with `tonic-build` 0.12.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p engram-coordinator \
    && cp target/release/engram-coordinator /tmp/engram-coordinator

# Trixie matches the glibc version `rust:1.95-slim` builds against
# (currently 2.39). bookworm (2.36) would refuse to load the binary
# with: `version GLIBC_2.39 not found`.
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /tmp/engram-coordinator /usr/local/bin/engram-coordinator
COPY deploy/migrations /opt/engram/migrations
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/engram-coordinator"]
