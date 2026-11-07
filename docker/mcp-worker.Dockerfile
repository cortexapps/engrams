# The official Node 24 LTS release is pinned in both stages.
FROM node:24.22.2-bookworm-slim AS build
WORKDIR /app
COPY mcp-worker/package.json mcp-worker/package-lock.json ./
RUN npm ci --ignore-scripts
COPY mcp-worker/tsconfig.json mcp-worker/tsconfig.build.json ./
COPY mcp-worker/src ./src
RUN npm run build && npm prune --omit=dev --ignore-scripts

FROM node:24.22.2-bookworm-slim
ENV NODE_ENV=production
WORKDIR /app
COPY --from=build --chown=node:node /app/package.json ./
COPY --from=build --chown=node:node /app/node_modules ./node_modules
COPY --from=build --chown=node:node /app/dist ./dist
USER node
EXPOSE 8443
# This is inherited by disposable schema children. No core or heap dump is kept.
ENTRYPOINT ["/bin/sh", "-c", "ulimit -c 0; exec node --max-old-space-size=256 dist/main.js"]
