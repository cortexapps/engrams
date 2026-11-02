import { readFileSync } from "node:fs";
import { expect, it } from "vitest";

it("sets the referrer policy before loading callback assets and config", () => {
  const html = readFileSync("index.html", "utf8");
  const policy = html.indexOf('<meta name="referrer" content="no-referrer"');
  expect(policy).toBeGreaterThanOrEqual(0);
  expect(policy).toBeLessThan(html.indexOf('<link rel="icon"'));
  expect(policy).toBeLessThan(html.indexOf('<script src="/engram-config.js"'));
  expect(policy).toBeLessThan(html.indexOf('<script type="module"'));
});
