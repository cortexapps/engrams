# syntax=docker/dockerfile:1.7
#
# nginx + the React SPA from web/. The chart mounts a ConfigMap at
# /etc/nginx/conf.d/default.conf (SPA routing + reverse-proxy to the
# coord), so this image intentionally ships no server config — the
# chart owns that surface. Built from the repo root:
#
#   docker buildx build -f docker/web.Dockerfile -t engram/web:dev .

FROM node:25-alpine AS builder
WORKDIR /src

# pnpm v9, matching the CI web lane (pnpm/action-setup version 9 in
# .github/workflows/ci.yml) so local + CI builds agree.
#
# Installed directly rather than through corepack: Node stopped shipping
# corepack in v25 (nodejs/node#57617), so `corepack enable` is a dead end
# — it would break again at the next even LTS. npm is in the base image.
RUN npm install -g pnpm@9

# Lockfile first so the install layer stays warm across source edits.
COPY web/package.json web/pnpm-lock.yaml ./
RUN --mount=type=cache,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile

# Sources next; `pnpm build` is `tsc -b && vite build` per
# web/package.json, output lands in ./dist.
COPY web/ ./
RUN pnpm build

FROM nginx:1.31-alpine
# Drop the upstream default config — the chart provides it via
# ConfigMap mount (deploy/helm/engram/templates/web-configmap.yaml).
# Keeping it would leave a stale 80/SPA-only fallback inside the
# image if the mount ever misfires.
RUN rm -f /etc/nginx/conf.d/default.conf
COPY --from=builder /src/dist /usr/share/nginx/html
# nginx:alpine's `nginx` user is UID 101; the chart runs the pod as
# that user and listens on a high port (8080) to avoid needing
# CAP_NET_BIND_SERVICE.
EXPOSE 8080
