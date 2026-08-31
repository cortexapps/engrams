// Runtime deployment config, loaded synchronously before the SPA bundle.
// This default (same-origin API) ships inside the image; a split-host
// deployment overrides this exact path from the web chart's nginx config
// (`web.apiBaseUrl`) — see deploy/helm/engram/templates/web-configmap.yaml
// and web/src/lib/base.ts for what the value does.
window.__ENGRAM_API_ORIGIN__ = "";
