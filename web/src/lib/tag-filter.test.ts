import { describe, expect, it } from "vitest";

import { NO_TAGS, parseTagSearch } from "./tag-filter";

describe("parseTagSearch", () => {
  it("reads one name or a list of names", () => {
    expect(parseTagSearch({ tags: "Ops" })).toEqual(["Ops"]);
    expect(parseTagSearch({ tags: ["Ops", "data"] })).toEqual(["Ops", "data"]);
  });

  it("drops values that are not names and repeated names, keeping order", () => {
    expect(parseTagSearch({ tags: ["Ops", 3, "Ops", null, "data"] })).toEqual(["Ops", "data"]);
    expect(parseTagSearch({ tags: { x: 1 } })).toEqual([]);
    expect(parseTagSearch({ tags: 7 })).toEqual([]);
    expect(parseTagSearch({})).toEqual([]);
  });

  it("keeps the empty name, which stands for No tags", () => {
    expect(parseTagSearch({ tags: [""] })).toEqual([NO_TAGS]);
    expect(NO_TAGS).toBe("");
  });
});
