# syntax=docker/dockerfile:1.7
# ADR 0044 K3: the host-fleet rollout operator — a plain controller binary
# (K8s API + coordinator HTTP, rustls, no privileged surface).
FROM rust:1.97-slim AS builder
WORKDIR /src
# protobuf-compiler/pkg-config aren't needed by the operator's own closure
# (rustls, no -sys C deps), but keep the builder consistent with the other
# images so a transitive build.rs never surprises the bake.
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p engram-host-operator \
    && cp target/release/engram-host-operator /tmp/engram-host-operator

# Trixie matches the builder's glibc.
FROM debian:trixie-slim
# ca-certificates: TLS to the K8s API server + the coordinator.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /tmp/engram-host-operator /usr/local/bin/engram-host-operator
# Matches the chart's runAsUser; the binary needs no root.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/engram-host-operator"]
