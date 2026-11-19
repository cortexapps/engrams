import { describe, expect, it } from "vitest";

import { matchesTagFilter, NO_TAGS, orderTagsForDisplay, parseTagSearch } from "./tag-filter";

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

describe("matchesTagFilter", () => {
  it("matches everything when nothing is selected", () => {
    expect(matchesTagFilter(["Ops"], [])).toBe(true);
    expect(matchesTagFilter([], [])).toBe(true);
  });

  it("matches any selected name, without regard to case", () => {
    expect(matchesTagFilter(["Ops", "Billing"], ["billing"])).toBe(true);
    expect(matchesTagFilter(["Ops"], ["data", "OPS"])).toBe(true);
    expect(matchesTagFilter(["Ops"], ["data"])).toBe(false);
  });

  it("No tags matches only an automation without tags", () => {
    expect(matchesTagFilter([], [NO_TAGS])).toBe(true);
    expect(matchesTagFilter(["Ops"], [NO_TAGS])).toBe(false);
    expect(matchesTagFilter(["Ops"], [NO_TAGS, "ops"])).toBe(true);
  });
});

describe("orderTagsForDisplay", () => {
  it("puts selected tags first, without regard to case, and keeps the given order", () => {
    const tags = [{ name: "a" }, { name: "B" }, { name: "c" }];
    expect(orderTagsForDisplay(tags, ["b"]).map((t) => t.name)).toEqual(["B", "a", "c"]);
    expect(orderTagsForDisplay(tags, ["c", "a"]).map((t) => t.name)).toEqual(["a", "c", "B"]);
    expect(orderTagsForDisplay(tags, []).map((t) => t.name)).toEqual(["a", "B", "c"]);
  });
});
