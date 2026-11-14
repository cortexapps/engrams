# syntax=docker/dockerfile:1.7
# Test fixture only. The base is the production host image at an immutable
# manifest digest. Its revision label and entrypoint remain unchanged.
ARG HOST_IMAGE
FROM ${HOST_IMAGE}
ARG HOST_IMAGE
ARG FIRECRACKER_SHA256
RUN set -eu; \
    case "$HOST_IMAGE" in *@sha256:*) digest="${HOST_IMAGE##*@sha256:}" ;; *) exit 2 ;; esac; \
    test "${#digest}" = 64; \
    case "$digest" in *[!a-f0-9]*) exit 2 ;; esac
COPY --chmod=0755 firecracker /usr/local/bin/firecracker
RUN set -eu; \
    test "${#FIRECRACKER_SHA256}" = 64; \
    case "$FIRECRACKER_SHA256" in *[!a-f0-9]*) exit 2 ;; esac; \
    printf '%s  /usr/local/bin/firecracker\n' "$FIRECRACKER_SHA256" | sha256sum -c -
