import { expect, test } from "bun:test";

import { makeWebhookAliasResolver } from "../aliases.ts";

const NOW = new Date("2026-07-22T12:00:00Z");

test("an unknown registration resolves no aliases (the github-app special case is gone)", async () => {
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

  expect(await resolve("github-app")).toEqual([]);
  expect(registrationReads).toBe(1);
});

test("a registration's provider hint selects the connector's alias facet", async () => {
  const resolve = makeWebhookAliasResolver({
    registrations: {
      async getRegistration(id) {
        if (id !== "team-hooks") return null;
        return {
          id,
          name: "Team hooks",
          verification: { scheme: "generic_hmac_sha256", secretRef: "webhook.team-hooks.secret" },
          providerHint: "github",
          disabledReason: null,
          createdByUserId: "admin",
          createdAt: NOW,
          updatedAt: NOW,
        };
      },
    },
    connectors: { list: async () => [] },
  });

  expect(await resolve("team-hooks")).toContainEqual({
    path: "issue.title",
    alias: "issue.title",
  });
});
