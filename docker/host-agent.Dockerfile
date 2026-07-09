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
#   - e2fsprogs: mke2fs, to materialize the per-sandbox ext4 rootfs AND
#     (ADR 0080 §C) the enable-time image materialization. This is now
#     the ONLY mke2fs in the pipeline (the bake path retired), so its
#     determinism matters: ADR 0036 needs an e2fsprogs that honors
#     SOURCE_DATE_EPOCH (>= 1.47.1) or re-materializing unchanged content
#     stamps wall-clock times and breaks chunk dedup / base-snapshot
#     reuse. debian:trixie ships e2fsprogs 1.47.2 (matches the flake pin),
#     so the apt mke2fs is fine — but we ASSERT the floor below so a base
#     bump that regresses it fails the image build loudly.
#   - libarchive13t64: ADR 0084 tar-input materialization; trixie's mke2fs
#     dlopens libarchive for `-d <tarball>` support.
#   - iproute2:  ip, for the tap device + guest netns
#   - iptables:  egress NAT / firewall rules for guest networking
# (ca-certificates: TLS to the blob backend — GCS/S3.)
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates e2fsprogs libarchive13t64 iproute2 iptables \
    && rm -rf /var/lib/apt/lists/*
# ADR 0080/0036/0084: fail the build if the base's mke2fs predates
# SOURCE_DATE_EPOCH support or regresses tar-input libarchive support.
RUN set -eux; \
    ver="$(mke2fs -V 2>&1 | sed -n 's/^mke2fs \([0-9][0-9.]*\).*/\1/p' | head -1)"; \
    echo "e2fsprogs mke2fs: $ver"; \
    [ "$(printf '1.47.1\n%s\n' "$ver" | sort -V | head -1)" = "1.47.1" ] || { \
        echo "mke2fs $ver < 1.47.1 — deterministic ext4 pack broken (ADR 0036); pin a newer base or bundle a static mke2fs" >&2; \
        exit 1; \
    }; \
    scratch="$(mktemp -d)"; \
    trap 'rm -rf "$scratch"' EXIT; \
    mkdir -p "$scratch/root/bin"; \
    printf 'setuid smoke\n' > "$scratch/root/bin/setuid-probe"; \
    tar --numeric-owner --owner=0 --group=0 --mode=0755 --no-recursion -cf "$scratch/input.tar" -C "$scratch/root" . ./bin; \
    tar --numeric-owner --owner=0 --group=0 --mode=04755 -rf "$scratch/input.tar" -C "$scratch/root" ./bin/setuid-probe; \
    mke2fs -q -F -t ext4 -d "$scratch/input.tar" "$scratch/rootfs.ext4" 4m; \
    stat_out="$(debugfs -R "stat /bin/setuid-probe" "$scratch/rootfs.ext4" 2>/dev/null)"; \
    printf '%s\n' "$stat_out"; \
    printf '%s\n' "$stat_out" | grep -Eq 'User:[[:space:]]+0[[:space:]]+Group:[[:space:]]+0' || { \
        echo "mke2fs tar input failed to preserve uid/gid 0 from the tar header (ADR 0084)" >&2; \
        exit 1; \
    }; \
    printf '%s\n' "$stat_out" | grep -Eq 'Mode:[[:space:]]+04755' || { \
        echo "mke2fs tar input failed to preserve mode 04755 from the tar header (ADR 0084)" >&2; \
        exit 1; \
    }
# ADR 0080: host-agent's materializer resolves mke2fs via ENGRAM_MKE2FS
# first (engram_rootfs_materializer::ext4::resolve_mke2fs). Pin it at the
# apt path so the resolution never depends on PATH ordering or a sibling.
ENV ENGRAM_MKE2FS=/usr/sbin/mke2fs
COPY --from=builder /tmp/engram-host-agent /usr/local/bin/engram-host-agent
# ADR 0044 K2: the UFFD memory-restore handler. On the GCE/Packer hosts it
# came from the node image; the K8s host-agent image must carry it (a PATH
# lookup of `engram-uffd-handler` is the FirecrackerConfig default). Without
# it, every Uffd-mode restore/resume page-faults forever / fails to spawn.
COPY --from=builder /tmp/engram-uffd-handler /usr/local/bin/engram-uffd-handler
ENTRYPOINT ["/usr/local/bin/engram-host-agent"]
