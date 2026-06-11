# syntax=docker/dockerfile:1.7
# ADR 0044 K2 / GAP 2: a data-carrier image for the K8s host fleet.
#
# On the GCE/Packer hosts, `firecracker` + the guest kernel come baked into
# the node image. Stock K8s nodes have neither — so a host-agent pod init
# container copies them out of THIS image into a shared `emptyDir` (the
# engram-host-fleet chart wires that up). That keeps the FC binary lifecycle
# decoupled from the node and from the agent pod (each pod gets its own copy;
# a reattached FC keeps the running binary it was started with).
#
# The build context must contain ./firecracker, ./vmlinux, and ./bundles/
# (the ADR 0027 RO session bundles + current.json stamp) — produce them all
# with docker/node-assets-fetch.sh (pinned FC release + the engram guest
# kernel GH release asset + the bundle-*:main OCI artifacts). busybox gives
# the chart's init container a /bin/sh + cp + cmp for the idempotent copy.
FROM busybox:1.36
COPY firecracker /assets/firecracker
COPY vmlinux /assets/vmlinux
# ADR 0027: the skills/playwright squashfs + current.json stamp the host-agent
# stages to /var/lib/engram/shared.
COPY bundles /assets/bundles
RUN chmod 0755 /assets/firecracker && chmod 0644 /assets/vmlinux \
    && chmod -R a+r /assets/bundles
LABEL org.opencontainers.image.source=https://github.com/cortexapps/engrams
LABEL org.opencontainers.image.description="Firecracker binary + engram guest kernel + RO session bundles for the ADR 0044 K8s host fleet"
