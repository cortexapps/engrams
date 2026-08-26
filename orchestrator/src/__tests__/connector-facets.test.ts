/** ADR 0119 D5: the grown webhook facet (label/description/schema/hidden,
 * ingress, scope), the actions facet, and the sample fixtures. */

import { describe, expect, test } from "bun:test";

import {
  connectorRegistry,
  parseConnector,
  parseFieldSchema,
  type Connector,
  type FieldSchema,
} from "../connectors/registry.ts";
import { loadEventSample } from "../connectors/samples.ts";
import { ownPath } from "../automations/paths.ts";
import { redactWebhookPayload } from "../automations/webhook.ts";

const base = {
  provider: "github",
  protocol: "http",
  credential: { source: "mint", mint: { kind: "github_app" } },
  hosts: ["api.github.com"],
  operations: [
    { grants: ["issues:write"], match: { method: "POST", path: "/repos/*/issues" } },
  ],
};

const BUILTIN = { builtin: true } as const;

function webhook(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    verificationScheme: "github_hmac_sha256",
    events: [{ key: "issues.opened", label: "Issue opened" }],
    aliases: [],
    ...overrides,
  };
}

describe("grown webhook facet", () => {
  test("accepts label/description/schema/hidden, ingress, and scope", () => {
    const c = parseConnector(
      {
        ...base,
        webhook: webhook({
          events: [
            {
              key: "pull_request.opened",
              label: "Pull request opened",
              description: "A pull request was opened.",
              schema: {
                type: "object",
                properties: {
                  action: { type: "string" },
                  pull_request: {
                    type: "object",
                    properties: { number: { type: "integer" }, draft: { type: "boolean" } },
                  },
                },
              },
            },
            { key: "installation.created", label: "App installed", hidden: true },
          ],
          ingress: { scheme: "github_hmac_sha256", secretRef: "github.webhook_secret" },
          scope: { key: "repositories", label: "Repositories", path: "repository.full_name" },
        }),
      },
      "x",
      BUILTIN,
    );
    expect(c.webhook?.events[0]?.label).toBe("Pull request opened");
    expect(c.webhook?.events[1]?.hidden).toBe(true);
    expect(c.webhook?.ingress).toEqual({
      scheme: "github_hmac_sha256",
      secretRef: "github.webhook_secret",
    });
    expect(c.webhook?.scope?.path).toBe("repository.full_name");
  });

  test("rejects unknown ingress schemes and bad scope paths", () => {
    expect(() =>
      parseConnector(
        { ...base, webhook: webhook({ ingress: { scheme: "md5", secretRef: "x" } }) },
        "x",
        BUILTIN,
      ),
    ).toThrow(/ingress.scheme/);
    expect(() =>
      parseConnector(
        {
          ...base,
          webhook: webhook({
            scope: { key: "repositories", label: "Repos", path: "__proto__.full_name" },
          }),
        },
        "x",
        BUILTIN,
      ),
    ).toThrow(/scope.path/);
  });

  test("field schema bounds: depth, property budget, unsafe segments", () => {
    let deep: FieldSchema = { type: "string" };
    for (let i = 0; i < 9; i += 1) {
      deep = { type: "object", properties: { a: deep } };
    }
    expect(() => parseFieldSchema("x", deep)).toThrow(/deeper than 8/);

    const wide = {
      type: "object",
      properties: Object.fromEntries(
        Array.from({ length: 251 }, (_, i) => [`f${i}`, { type: "string" }]),
      ),
    };
    expect(() => parseFieldSchema("x", wide)).toThrow(/more than 250/);

    expect(() =>
      parseFieldSchema("x", { type: "object", properties: { __proto__constructor: { type: "string" } } }),
    ).not.toThrow();
    expect(() =>
      parseFieldSchema("x", {
        type: "object",
        properties: { "bad name": { type: "string" } },
      }),
    ).toThrow(/property name/);
    expect(() =>
      parseFieldSchema("x", { type: "object", properties: { a: { type: "function" } } }),
    ).toThrow(/unknown schema type/);
  });
});

describe("actions facet", () => {
  const action = (overrides: Record<string, unknown> = {}): Record<string, unknown> => ({
    id: "create_issue_comment",
    label: "Create an issue comment",
    inputSchema: {
      type: "object",
      properties: {
        repo: { type: "string" },
        number: { type: "integer" },
        body: { type: "string" },
      },
      required: ["repo", "number", "body"],
    },
    execute: {
      kind: "http",
      method: "POST",
      pathTemplate: "/repos/{input.repo}/issues/{input.number}/comments",
      bodyTemplate: { body: "{input.body}" },
    },
    idempotency: { kind: "marker_comment" },
    ...overrides,
  });

  test("accepts a valid http action", () => {
    const c = parseConnector({ ...base, actions: [action()] }, "x", BUILTIN);
    expect(c.actions?.[0]?.id).toBe("create_issue_comment");
  });

  test("rejects actions on custom (non-builtin) connectors", () => {
    expect(() => parseConnector({ ...base, actions: [action()] }, "db:x")).toThrow(
      /not allowed on custom connectors/,
    );
  });

  test("rejects undeclared placeholders, traversal, graphql without endpoint, unknown builtins", () => {
    expect(() =>
      parseConnector(
        {
          ...base,
          actions: [
            action({
              execute: {
                kind: "http",
                method: "POST",
                pathTemplate: "/repos/{input.nope}/comments",
              },
            }),
          ],
        },
        "x",
        BUILTIN,
      ),
    ).toThrow(/undeclared input/);
    expect(() =>
      parseConnector(
        {
          ...base,
          actions: [
            action({ execute: { kind: "http", method: "POST", pathTemplate: "/repos/../admin" } }),
          ],
        },
        "x",
        BUILTIN,
      ),
    ).toThrow(/must not contain/);
    expect(() =>
      parseConnector(
        { ...base, actions: [action({ execute: { kind: "graphql", document: "mutation { x }" } })] },
        "x",
        BUILTIN,
      ),
    ).toThrow(/graphqlEndpoint/);
    expect(() =>
      parseConnector(
        { ...base, actions: [action({ execute: { kind: "builtin", id: "github.rm_rf" } })] },
        "x",
        BUILTIN,
      ),
    ).toThrow(/registered builtin actions/);
  });

  test("rejects a body placeholder over an undeclared input", () => {
    expect(() =>
      parseConnector(
        {
          ...base,
          actions: [
            action({
              execute: {
                kind: "http",
                method: "POST",
                pathTemplate: "/repos/{input.repo}/issues",
                bodyTemplate: { title: "{input.title}" },
              },
            }),
          ],
        },
        "x",
        BUILTIN,
      ),
    ).toThrow(/undeclared input/);
  });
});

describe("handle templates (ADR 0120 instances)", () => {
  const event = (handleCandidates: unknown) => ({
    ...base,
    webhook: webhook({
      events: [{ key: "issues.opened", label: "Issue opened", handleCandidates }],
    }),
  });

  test("accepts the parts grammar on events and actions", () => {
    const c = parseConnector(
      event([
        { parts: [{ lit: "github:" }, { path: "repository.full_name" }, { lit: "#" }, { path: "issue.number" }] },
      ]),
      "x",
      BUILTIN,
    );
    expect(c.webhook?.events[0]?.handleCandidates?.[0]?.parts).toHaveLength(4);
  });

  test.each([
    [[], "must not be empty"],
    [[{ parts: [{ lit: "a:" }] }], "2.."],
    [[{ parts: [{ path: "a" }, { path: "b" }] }], "first part must be a literal"],
    [[{ parts: [{ lit: "a:" }, { lit: "b" }] }], "at least one part must be a path"],
    [[{ parts: [{ lit: "a:" }, { path: "__proto__.x" }] }], "payload path"],
    [[{ parts: [{ lit: "a:" }, { path: "x", lit: "y" }] }], "exactly"],
    [[{ parts: [{ lit: "" }, { path: "x" }] }], "non-empty string"],
  ])("rejects bad candidate grammar %#", (handleCandidates, message) => {
    expect(() => parseConnector(event(handleCandidates), "x", BUILTIN)).toThrow(message);
  });

  test("action handles pin the input./output. scope to declared fields", () => {
    const action = (handles: unknown) => ({
      ...base,
      actions: [
        {
          id: "post",
          label: "Post",
          inputSchema: { type: "object", properties: { channel: { type: "string" } } },
          execute: { kind: "http", method: "POST", pathTemplate: "/post" },
          output: { ts: "ts" },
          idempotency: { kind: "none" },
          handles,
        },
      ],
    });
    const ok = parseConnector(
      action([{ parts: [{ lit: "slack:" }, { path: "input.channel" }, { lit: ":" }, { path: "output.ts" }] }]),
      "x",
      BUILTIN,
    );
    expect(ok.actions?.[0]?.handles).toHaveLength(1);
    expect(() =>
      parseConnector(action([{ parts: [{ lit: "s:" }, { path: "input.nope" }] }]), "x", BUILTIN),
    ).toThrow("not an inputSchema property");
    expect(() =>
      parseConnector(action([{ parts: [{ lit: "s:" }, { path: "output.nope" }] }]), "x", BUILTIN),
    ).toThrow("not a declared output field");
    expect(() =>
      parseConnector(action([{ parts: [{ lit: "s:" }, { path: "raw.thing" }] }]), "x", BUILTIN),
    ).toThrow('"input.<field>" or "output.<field>"');
  });
});

describe("built-in seeds", () => {
  const registry = connectorRegistry();
  const providers: Array<[string, Connector]> = ["github", "slack", "linear"].map((p) => [
    p,
    registry.get(p)!,
  ]);

  test("all seeds still load; github/slack/linear carry ingress + scope + actions", () => {
    for (const [provider, connector] of providers) {
      expect(connector, provider).toBeDefined();
      expect(connector.webhook?.ingress, provider).toBeDefined();
      expect(connector.webhook?.scope, provider).toBeDefined();
      expect(connector.actions?.length ?? 0, provider).toBeGreaterThan(0);
    }
  });

  test("slack + github declare handle candidates on their routable events", () => {
    const slackEvents = registry.get("slack")!.webhook!.events;
    for (const key of ["app_mention", "message", "reaction_added"]) {
      expect(slackEvents.find((e) => e.key === key)?.handleCandidates, key).toBeDefined();
    }
    const githubEvents = registry.get("github")!.webhook!.events;
    for (const event of githubEvents) {
      const routable =
        event.key.startsWith("pull_request") ||
        event.key.startsWith("issues.") ||
        event.key === "issue_comment.created";
      expect(event.handleCandidates !== undefined, event.key).toBe(routable);
    }
  });

  test("every non-hidden declared event has a redaction-stable sample fixture", () => {
    for (const [provider, connector] of providers) {
      for (const event of connector.webhook?.events ?? []) {
        if (event.hidden) continue;
        const sample = loadEventSample(provider, event.key);
        expect(sample, `${provider}/${event.key}`).not.toBeNull();
        // The checked-in fixture must already be clean: redaction is a no-op.
        expect(redactWebhookPayload(sample!), `${provider}/${event.key}`).toEqual(sample!);
      }
    }
  });

  test("hidden events have no picker sample requirement and unknown keys return null", () => {
    expect(loadEventSample("github", "not.declared")).toBeNull();
    expect(loadEventSample("../evil", "x")).toBeNull();
  });

  test("every declared event resolves at least one connector alias from its own sample", () => {
    // Aliases are connector-wide; an event whose sample resolves none of them
    // gives templates nothing curated to reference (the reaction_added gap).
    for (const [provider, connector] of providers) {
      const aliases = connector.webhook?.aliases ?? [];
      for (const event of connector.webhook?.events ?? []) {
        if (event.hidden) continue;
        const sample = loadEventSample(provider, event.key);
        if (sample === null) continue;
        const resolved = aliases.filter((mapping) => ownPath(sample, mapping.path) !== undefined);
        expect(resolved.length, `${provider}/${event.key} resolves no aliases`).toBeGreaterThan(0);
      }
    }
  });
});
