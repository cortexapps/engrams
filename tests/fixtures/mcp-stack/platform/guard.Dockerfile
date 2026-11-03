FROM debian:trixie-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4
ARG SOURCE_COMMIT
LABEL org.opencontainers.image.revision=${SOURCE_COMMIT}
RUN set -eu; \
    test "${#SOURCE_COMMIT}" = 40; \
    case "$SOURCE_COMMIT" in *[!a-f0-9]*) exit 2 ;; esac; \
    rm -f /etc/apt/sources.list /etc/apt/sources.list.d/debian.sources; \
    printf '%s\n' \
      'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20261102T000000Z trixie main' \
      'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian-security/20261102T000000Z trixie-security main' \
      > /etc/apt/sources.list; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      bash coreutils findutils ca-certificates nftables bind9-dnsutils \
      netcat-openbsd curl skopeo squashfs-tools iproute2 openssl; \
    rm -rf /var/lib/apt/lists/*; \
    for tool in /bin/sh /bin/bash /bin/sleep /bin/mkdir /bin/chown /bin/chmod \
      /usr/bin/find /usr/bin/sha256sum /usr/bin/skopeo /usr/sbin/nft \
      /usr/bin/dig /usr/bin/nc /usr/bin/curl /usr/bin/mksquashfs \
      /usr/bin/dirname /usr/bin/rm /usr/bin/cut /usr/sbin/ip /usr/bin/openssl; do test -x "$tool"; done
# Numeric packing users need no passwd entry or private home directory.
ENV HOME=/tmp
USER 1000:1000
ENTRYPOINT ["/bin/sleep"]
CMD ["infinity"]
