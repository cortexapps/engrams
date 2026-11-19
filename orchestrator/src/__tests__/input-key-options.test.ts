import { describe, expect, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";

import type { IntegrationConnectionRow } from "../db/integration-connections.ts";
import { productionInputKeyOptions } from "../rpc/automations.ts";

function source(rows: Array<Pick<IntegrationConnectionRow, "id" | "provider">>) {
  const asked: string[] = [];
  const options = productionInputKeyOptions(
    () => ({
      async listObservedScopeValues(connectionId) {
        asked.push(connectionId);
        return [`repo-of-${connectionId}`];
      },
    }),
    () => ({
      get: async (id) => (rows.find((row) => row.id === id) as IntegrationConnectionRow | undefined) ?? null,
      getDefault: async (provider) =>
        (rows.find((row) => row.provider === provider) as IntegrationConnectionRow | undefined) ?? null,
    }),
  );
  return { options, asked };
}

describe("the keys offered for a map input", () => {
  const rows = [
    { id: "c-github", provider: "github" },
    { id: "c-gitlab", provider: "gitlab" },
    { id: "c-slack", provider: "slack" },
  ];

  test("repositories come from the trigger's connection when it is a forge", async () => {
    const { options, asked } = source(rows);
    expect(await options.list("repository", "c-gitlab")).toEqual([
      { key: "repo-of-c-gitlab", label: "repo-of-c-gitlab" },
    ]);
    expect(asked).toEqual(["c-gitlab"]);
  });

  test("a trigger of another role gives the default forge connection, not its own ledger", async () => {
    const { options, asked } = source(rows);
    expect(await options.list("repository", "c-slack")).toEqual([
      { key: "repo-of-c-github", label: "repo-of-c-github" },
    ]);
    expect(asked).toEqual(["c-github"]);
  });

  test("a connection that does not exist is an error", async () => {
    const { options } = source(rows);
    const error = await options.list("repository", "c-none").catch((e: unknown) => e);
    expect(error).toBeInstanceOf(ConnectError);
    expect((error as ConnectError).code).toBe(Code.NotFound);
  });
});
