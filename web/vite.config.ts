import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// Coordinator binds to 127.0.0.1:8090 in `just dev` / `just dev-vz`.
// Proxy /sessions and /api straight through so the SPA hits same-origin.
const COORDINATOR = process.env.ENGRAM_COORDINATOR_URL ?? 'http://127.0.0.1:8090';

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    port: 5173,
    proxy: {
      // The SPA's URL scheme overlaps the coordinator's REST namespace
      // (e.g. /sessions/:id is both a SPA route and an API endpoint),
      // so we differentiate by Accept header. Browser navigation —
      // refresh, deep link, copy/paste — sends `Accept: text/html`,
      // which we shunt to /index.html so React Router takes over.
      // Same-origin fetches from the SPA send `application/json` and
      // SSE clients send `text/event-stream`; both fall through and
      // proxy to the coordinator unchanged.
      '/sessions': {
        target: COORDINATOR,
        changeOrigin: true,
        // The shell endpoint is a WebSocket; without this flag http-proxy
        // returns 426 Upgrade Required and the upgrade never completes.
        ws: true,
        bypass(req) {
          // WebSocket upgrade requests carry `Upgrade: websocket` —
          // never shunt those to index.html, no matter what their
          // Accept header says.
          const upgrade = (req.headers.upgrade ?? '').toString().toLowerCase();
          if (upgrade === 'websocket') return undefined;
          if (
            req.method === 'GET' &&
            (req.headers.accept ?? '').includes('text/html')
          ) {
            return '/index.html';
          }
        },
      },
      '/api': { target: COORDINATOR, changeOrigin: true },
      '/healthz': { target: COORDINATOR, changeOrigin: true },
    },
  },
});
