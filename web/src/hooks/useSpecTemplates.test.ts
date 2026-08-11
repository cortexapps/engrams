import { afterEach, describe, expect, test, vi } from "vitest";

import {
  cloneSpecTemplate,
  listSpecTemplates,
  restoreSpecTemplate,
  saveSpecTemplate,
  type SpecTemplateDefinition,
} from "./useSpecTemplates";

const definition: SpecTemplateDefinition = {
  name: "Design",
  description: "",
  layers: [{ key: "intent", title: "Intent" }],
  sections: [
    {
      key: "problem",
      title: "Problem",
      layerKey: "intent",
      guidance: "",
      doneCriteria: [],
      required: true,
      allowNa: false,
    },
  ],
  stageFlags: { alternatives: "on", talkItThrough: "suggested", gapCheck: "on" },
};

afterEach(() => vi.unstubAllGlobals());

describe("spec template requests", () => {
  test("lists the organization catalog", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(json({ templates: [{ id: "template-1" }] }))),
    );

    expect(await listSpecTemplates()).toEqual([{ id: "template-1" }]);
  });

  test("uses create and update routes for saves", async () => {
    const fetchMock = vi.fn(() => Promise.resolve(json({ template: { id: "template-1" } })));
    vi.stubGlobal("fetch", fetchMock);

    await saveSpecTemplate(null, definition);
    await saveSpecTemplate("template-1", definition);

    expect(fetchMock).toHaveBeenNthCalledWith(
      1,
      "/api/v1/spec-templates",
      expect.objectContaining({ method: "POST" }),
    );
    expect(fetchMock).toHaveBeenNthCalledWith(
      2,
      "/api/v1/spec-templates/template-1",
      expect.objectContaining({ method: "PUT" }),
    );
  });

  test("uses explicit clone and restore actions", async () => {
    const fetchMock = vi.fn(() => Promise.resolve(json({ template: { id: "template-1" } })));
    vi.stubGlobal("fetch", fetchMock);

    await cloneSpecTemplate("template-1");
    await restoreSpecTemplate("template-1");

    expect(fetchMock).toHaveBeenNthCalledWith(
      1,
      "/api/v1/spec-templates/template-1/clone",
      expect.objectContaining({ method: "POST" }),
    );
    expect(fetchMock).toHaveBeenNthCalledWith(
      2,
      "/api/v1/spec-templates/template-1/restore",
      expect.objectContaining({ method: "POST" }),
    );
  });
});

function json(value: unknown): Response {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}
