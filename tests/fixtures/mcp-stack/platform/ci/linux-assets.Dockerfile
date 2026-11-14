# Test model CLI only. Harnesses, agentd and bundle leaves are production source.
FROM rust:1.99-alpine@sha256:7e9e2d068a107a2adafd19930011178d14a78326053cd4c3a9f7ad3fd049557d AS build
RUN apk add --no-cache build-base protobuf-dev clang-dev openssl-dev openssl-libs-static perl linux-headers bash curl squashfs-tools
WORKDIR /src
COPY . .
ARG TARGETARCH
ENV SQLX_OFFLINE=true \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=gcc \
    CC_x86_64_unknown_linux_musl=gcc \
    CC_aarch64_unknown_linux_musl=gcc \
    OPENSSL_STATIC=1
RUN set -eu; case "$TARGETARCH" in amd64) target=x86_64-unknown-linux-musl ;; arm64) target=aarch64-unknown-linux-musl ;; *) exit 2 ;; esac; \
    cargo build --locked --release --target "$target" -p engram-agentd -p engram-harness-claude -p engram-harness-codex; \
    cargo build --locked --release --target "$target" --manifest-path tests/fixtures/mcp-model-cli/Cargo.toml --bin engram-mcp-model-fixture; \
    mkdir /out; \
    cp "target/$target/release/engram-harness-claude" /out/claudeHarness; \
    cp "target/$target/release/engram-harness-codex" /out/codexHarness; \
    cp "tests/fixtures/mcp-model-cli/target/$target/release/engram-mcp-model-fixture" /out/modelCli; \
    deploy/bundles/agentd/build.sh "target/$target/release/engram-agentd" /out/agentdBundle; \
    deploy/bundles/sentinel/build.sh /out/sentinelBundle; \
    deploy/bundles/guest-tools/build.sh /out/guestToolsBundle "$TARGETARCH"
FROM scratch
COPY --from=build /out/ /
