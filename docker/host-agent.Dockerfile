# syntax=docker/dockerfile:1.7
FROM rust:1.83-slim AS builder
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p engram-host-agent \
    && cp target/release/engram-host-agent /tmp/engram-host-agent

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /tmp/engram-host-agent /usr/local/bin/engram-host-agent
ENTRYPOINT ["/usr/local/bin/engram-host-agent"]
