import { expect, test } from "bun:test";

import { makeWebhookAliasResolver } from "../aliases.ts";

test("github-app resolves the built-in GitHub facet without a registration row", async () => {
  let registrationReads = 0;
  const resolve = makeWebhookAliasResolver({
    registrations: {
      async getRegistration() {
        registrationReads++;
        return null;
      },
    },
    connectors: { list: async () => [] },
  });

  const aliases = await resolve("github-app");
  expect(registrationReads).toBe(0);
  expect(aliases).toContainEqual({
    path: "issue.title",
    alias: "issue.title",
  });
});
