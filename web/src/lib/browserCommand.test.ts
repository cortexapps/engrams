import { describe, expect, test } from "vitest";

import { isSharedBrowserCommand } from "./browserCommand";

describe("isSharedBrowserCommand", () => {
  test.each([
    [["agent-browser", "open", "http://localhost:3000"]],
    [["/usr/local/bin/playwright-cli", "snapshot"]],
    [["bash", "-lc", "agent-browser snapshot -i"]],
  ])("recognizes shared browser commands", (command) => {
    expect(isSharedBrowserCommand(command)).toBe(true);
  });

  test.each([
    [["echo", "playwright-cli-notes"]],
    [["rg", "agent-browser", "README.md"]],
    [["engram-browser", "--ensure"]],
  ])("ignores unrelated commands", (command) => {
    expect(isSharedBrowserCommand(command)).toBe(false);
  });
});
