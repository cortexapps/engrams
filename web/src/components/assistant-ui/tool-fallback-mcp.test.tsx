import { afterEach, expect, test, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { ToolFallbackResult } from "./tool-fallback";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

test("raw MCP result text and structured content are inert and retain error data", () => {
  const fetch = vi.spyOn(globalThis, "fetch");
  const result = {
    content: [
      {
        type: "text",
        text: '![image](https://example.test/image) <img src="https://example.test/x">',
      },
    ],
    structuredContent: { url: "https://example.test/resource" },
    isError: true,
  };
  const { container } = render(<ToolFallbackResult result={result} />);
  expect(screen.getByText(/structuredContent/).textContent).toBe(JSON.stringify(result, null, 2));
  expect(container.querySelector("img, a, iframe")).toBeNull();
  expect(fetch).not.toHaveBeenCalled();
});
