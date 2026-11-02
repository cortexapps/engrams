# MCP worker foundation

This is a standalone Node 24 TypeScript ESM package. The official client is pinned
to 2.4.0. Ajv 8.20.0 and ipaddr.js are pinned for the worker validation boundary.
`package-lock.json` records exact dependency versions and integrity hashes.

There is no production service entrypoint or worker policy in this foundation.
The empty source module reserves the package build boundary. Later changes add
fixed authenticated RPCs and network policy. The feature remains disabled.

```sh
npm ci --ignore-scripts
npm run format:check
npm run typecheck
npm test
npm run build
```

`test/fixtures/server.ts` exposes `startMcpFixture(options)`. Each server binds
only loopback on an operating-system assigned port. It exposes `url`, `origin`,
`fetch`, `ca`, request observations, read/write/cancellation counts,
`setCatalogRevision`, `waitForCall`, `waitForCancellation`, `releaseCalls`, and
`close`. Always close fixtures in test cleanup.

Options select the protocol version, JSON/SSE replies, a held call barrier,
authentication, a callback issuer fault, and normal/malformed/oversize/binary/
image/audio/resource/resource-link/error results. Held calls release explicitly;
no sleep or wall-clock delay is needed. A write changes the counter before a
held or hostile reply. Cancellation does not roll that effect back.

The mock authorization server uses fixed public/confidential test clients and
real TLS. It publishes resource and authorization-server metadata, requires
PKCE S256 and exact resource/redirect/client matches, emits callback `iss`, and
rejects code/refresh replay and scope expansion. Confidential-client credentials
are explicit test strings. It is a test provider, not a credential authority for
the product.

`test/fixtures/tls.ts` contains a public test CA, certificate, and private key,
valid until 2036. `http.ts` trusts that CA explicitly for fixture requests and
never changes global TLS settings. Never import any fixture module into
production. The loopback listener and private CA do not create production
network exceptions. The mock server is handwritten so tests can send invalid
wire data. Tests use the actual official SDK to check the supported happy paths.
