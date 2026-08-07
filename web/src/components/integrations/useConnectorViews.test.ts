import { describe, expect, test } from "vitest";

import { operationsForEndpoints } from "./useConnectorViews";

describe("operationsForEndpoints", () => {
  test("offers Cloud SQL only when the connection names an instance", () => {
    const capabilities = [
      {
        action: "cloudsql.postgres.connect",
        access: "write" as const,
        label: "Connect to Cloud SQL PostgreSQL",
      },
    ];

    expect(operationsForEndpoints(capabilities, [], false)).toEqual([]);
    expect(operationsForEndpoints(capabilities, [], true)).toEqual(capabilities);
  });
});
