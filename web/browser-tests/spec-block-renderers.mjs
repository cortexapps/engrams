import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright";
import { createServer } from "vite";

const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const helmConfig = readFileSync(
  path.resolve(webRoot, "../deploy/helm/engram/templates/web-configmap.yaml"),
  "utf8",
);
const cspValues = [
  ...helmConfig.matchAll(/add_header Content-Security-Policy "([^"]+)" always;/g),
].map((match) => match[1]);
const productionCsp = cspValues.find((value) => value.includes("worker-src"));
const d2WorkerCsp = cspValues.find((value) => value.includes("'unsafe-eval'"));
assert(productionCsp, "The production web CSP header was not found.");
assert(d2WorkerCsp, "The production D2 worker CSP header was not found.");
assert.match(productionCsp, /worker-src 'self';/);
assert.doesNotMatch(productionCsp, /worker-src[^;]*blob:/);
assert.doesNotMatch(productionCsp, /'unsafe-eval'/);
assert.match(d2WorkerCsp, /script-src 'self' 'unsafe-eval' 'wasm-unsafe-eval'/);

const server = await createServer({
  root: webRoot,
  optimizeDeps: { force: true },
  server: {
    host: "127.0.0.1",
    port: 0,
    strictPort: false,
    headers: { "Content-Security-Policy": productionCsp },
  },
});

let browser;
try {
  await server.listen();
  const address = server.httpServer?.address();
  assert(address && typeof address === "object", "The browser test server did not start.");
  const origin = `http://127.0.0.1:${address.port}`;
  const localBrowser = "/usr/local/bin/engram-chromium";
  browser = await chromium.launch({
    headless: true,
    executablePath:
      process.env.ENGRAM_BROWSER_EXECUTABLE ??
      (existsSync(localBrowser) ? localBrowser : undefined),
  });
  const page = await browser.newPage();
  const externalRequests = [];
  const pageErrors = [];
  let d2WorkerStarted = false;
  page.on("pageerror", (error) => pageErrors.push(error.message));
  await page.route("**/*", async (route) => {
    const requestUrl = route.request().url();
    if (requestUrl.startsWith(origin)) {
      const pathname = new URL(requestUrl).pathname;
      const isD2Worker = pathname === "/d2/d2-worker.js";
      if (isD2Worker) {
        d2WorkerStarted = true;
        const response = await route.fetch();
        await route.fulfill({
          response,
          headers: { ...response.headers(), "content-security-policy": d2WorkerCsp },
        });
      } else {
        await route.continue();
      }
    } else {
      externalRequests.push(requestUrl);
      await route.abort("blockedbyclient");
    }
  });

  const response = await page.goto(`${origin}/browser-tests/spec-block-renderers.html`);
  assert.equal(response?.headers()["content-security-policy"], productionCsp);
  await page.waitForFunction(() => typeof window.runSpecBlockRendererTests === "function");
  const result = await page.evaluate(() => window.runSpecBlockRendererTests());

  assert.deepEqual(result.rendered, { mermaid: true, d2: true, flint: true });
  assert.equal(d2WorkerStarted, true);
  assert.deepEqual(result.deterministic, { mermaid: true, d2: true, flint: true });
  assert.equal(result.unsafeRejected, true);
  assert.equal(result.svgVectorsRejected, true);
  assert.equal(result.fallbackRendered, true);

  await page.evaluate(() => window.mountSpecBlockIterationTest());
  await page.getByRole("img", { name: "Mermaid diagram" }).click();
  await page
    .getByRole("textbox", { name: "Message about block request-flow", exact: true })
    .fill("Add a bounded retry path.");
  await page.getByRole("button", { name: "Send message about block request-flow" }).click();
  await page.waitForFunction(() => window.readSpecBlockIterationRequest() !== null);
  assert.deepEqual(await page.evaluate(() => window.readSpecBlockIterationRequest()), {
    sectionId: "design",
    blockId: "request-flow",
    message: "Add a bounded retry path.",
  });
  await page.getByText("Pinned to block").waitFor();
  assert.deepEqual(externalRequests, []);
  assert.deepEqual(pageErrors, []);
} finally {
  await browser?.close();
  await server.close();
}
