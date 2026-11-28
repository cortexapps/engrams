FROM node:24.22.2-bookworm-slim@sha256:c86ecead9e55a5210ac1727de38a4ca840bf03dcaf34dc9749addbf383b1920c AS source
WORKDIR /app
COPY tests/fixtures/mcp-stack/main.mts tests/fixtures/mcp-stack/config.mts tests/fixtures/mcp-stack/durable.mts ./tests/fixtures/mcp-stack/
COPY mcp-worker/package.json ./mcp-worker/
COPY mcp-worker/test/fixtures/server.ts mcp-worker/test/fixtures/http.ts mcp-worker/test/fixtures/oauth.ts ./mcp-worker/test/fixtures/
# The configured entrypoint requires runtime TLS. Do not package static test keys.
RUN printf '%s\n' \
    'export const TEST_CA = "";' \
    'export const TEST_CERT = "";' \
    'export const TEST_KEY = "";' \
    > mcp-worker/test/fixtures/tls.ts

FROM node:24.22.2-bookworm-slim@sha256:c86ecead9e55a5210ac1727de38a4ca840bf03dcaf34dc9749addbf383b1920c
ARG SOURCE_COMMIT
LABEL org.opencontainers.image.revision=${SOURCE_COMMIT}
RUN set -eu; \
    test "${#SOURCE_COMMIT}" = 40; \
    case "$SOURCE_COMMIT" in *[!a-f0-9]*) exit 2 ;; esac
ENV NODE_ENV=production
WORKDIR /app
COPY --from=source /app/tests/fixtures/mcp-stack/ ./tests/fixtures/mcp-stack/
COPY --from=source /app/mcp-worker/package.json ./mcp-worker/
COPY --from=source /app/mcp-worker/test/fixtures/ ./mcp-worker/test/fixtures/
USER node
ENTRYPOINT ["/usr/local/bin/node", "/app/tests/fixtures/mcp-stack/main.mts"]
