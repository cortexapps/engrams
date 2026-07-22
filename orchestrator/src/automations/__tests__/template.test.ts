import { describe, expect, test } from "bun:test";

import {
  AutomationTemplateError,
  appendAutomationEventContext,
  buildAutomationTemplateContext,
  renderAutomationAction,
  renderAutomationTemplate,
  validateAutomationTemplate,
} from "../template.ts";

const context = buildAutomationTemplateContext({
  automationName: "Issue triage",
  triggerKind: "webhook",
  receivedAt: "2026-07-22T12:00:00Z",
  eventKey: "issues.opened",
  rawPayload: {
    issue: { title: "Broken build", number: 42 },
    repository: { full_name: "cortexapps/engrams" },
    actor: { login: "octocat" },
  },
  aliases: [
    { path: "issue.title", alias: "issue.title" },
    { path: "issue.number", alias: "issue.number" },
    { path: "repository.full_name", alias: "repository.full_name" },
    { path: "actor.login", alias: "actor.login" },
  ],
});

describe("automation templates", () => {
  test("a missing variable is a render error", async () => {
    await expect(renderAutomationTemplate("${{ missing }}", context)).rejects.toMatchObject({
      code: "render_failed",
    });
  });

  test("a missing variable immediately before default renders the default", async () => {
    expect(
      await renderAutomationTemplate('${{ missing | default: "fallback" }}', context),
    ).toBe("fallback");
  });

  test("raw is the only enabled tag and protects literal output syntax", async () => {
    expect(
      await renderAutomationTemplate("{% raw %}${{ literal }}{% endraw %}", context),
    ).toBe("${{ literal }}");
  });

  test("curated aliases and event.raw are both available", async () => {
    expect(
      await renderAutomationTemplate(
        "#${{ event.issue.number }} ${{ event.issue.title | upcase }} in ${{ event.repository.full_name }} by ${{ event.actor.login }} / ${{ event.raw.issue.title }}",
        context,
      ),
    ).toBe("#42 BROKEN BUILD in cortexapps/engrams by octocat / Broken build");
  });

  test("alias construction rejects prototype-polluting paths", () => {
    expect(() =>
      buildAutomationTemplateContext({
        automationName: "Unsafe",
        triggerKind: "webhook",
        receivedAt: "2026-07-22T12:00:00Z",
        rawPayload: { issue: { title: "x" } },
        aliases: [{ path: "issue.title", alias: "__proto__.polluted" }],
      }),
    ).toThrow(/invalid webhook alias/);
    expect(Object.prototype).not.toHaveProperty("polluted");
  });

  test("payload template syntax is rendered exactly once", async () => {
    const injected = buildAutomationTemplateContext({
      automationName: "Once",
      triggerKind: "webhook",
      receivedAt: "2026-07-22T12:00:00Z",
      eventKey: "push",
      rawPayload: { text: "${{ trigger.automation.name }} {% raw %}oops{% endraw %}" },
      aliases: [{ path: "text", alias: "message.text" }],
    });
    expect(await renderAutomationTemplate("${{ event.message.text }}", injected)).toBe(
      "${{ trigger.automation.name }} {% raw %}oops{% endraw %}",
    );
  });

  test("caps direct rendered output", async () => {
    await expect(
      renderAutomationTemplate("${{ event.raw | json }}", context, { maxOutputChars: 10 }),
    ).rejects.toMatchObject({ code: "output_too_long" });
  });

  test("caps the final prompt after auto event context is appended", async () => {
    await expect(
      renderAutomationAction(
        {
          kind: "create_task",
          profileId: "profile-1",
          promptTemplate: "Triage",
          includeEventContext: true,
        },
        context,
        { maxOutputChars: 40 },
      ),
    ).rejects.toMatchObject({ code: "output_too_long" });
  });

  test("the auto context block identifies untrusted data", () => {
    const block = appendAutomationEventContext("Triage this", {
      automationName: "Issue triage",
      eventKey: "issues.opened",
      redactedPayload: { issue: { number: 42 } },
    });
    expect(block).toContain('--- Event context (automation "Issue triage", issues.opened) ---');
    expect(block).toContain("untrusted external input");
    expect(block).toContain('"number": 42');
  });

  test("save-time validation rejects unknown filters", () => {
    expect(() => validateAutomationTemplate("${{ event.issue.title | escape }}")).toThrow(
      AutomationTemplateError,
    );
  });

  test("save-time validation rejects every tag except raw", () => {
    expect(() => validateAutomationTemplate("{% assign x = 1 %}${{ x }}")).toThrow(
      AutomationTemplateError,
    );
    expect(() => validateAutomationTemplate('{% include "secret" %}')).toThrow(
      AutomationTemplateError,
    );
  });

  test("the approved filter set stays usable", async () => {
    expect(
      await renderAutomationTemplate(
        '${{ event.issue.title | downcase | truncate: 7 }} / ${{ event.raw.issue.number | json }} / ${{ event.raw.labels | default: empty | join: "," }}',
        context,
      ),
    ).toBe("brok... / 42 / ");
  });
});
