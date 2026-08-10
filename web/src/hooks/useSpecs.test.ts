import { describe, expect, test } from "vitest";

import { SPEC_LIST_QUERY_OPTIONS } from "./useSpecs";

describe("spec list query options", () => {
  test("refreshes mutable list fields on a bounded interval", () => {
    expect(SPEC_LIST_QUERY_OPTIONS).toMatchObject({
      refetchInterval: 15_000,
      refetchIntervalInBackground: false,
    });
  });
});
