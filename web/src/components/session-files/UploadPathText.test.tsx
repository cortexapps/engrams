import { render, screen } from "@testing-library/react";
import { describe, expect, test } from "vitest";

import { SessionFileContext, UploadPathChildren } from "./UploadPathText";

describe("transcript upload chips", () => {
  test("renders a canonical path as an authenticated session download", () => {
    const path = "/tmp/uploads/019fe2ff-0464-75f3-bb20-a8c1844579b9/notes.txt";
    render(
      <SessionFileContext.Provider value="session-1">
        <UploadPathChildren>{`See ${path} now.`}</UploadPathChildren>
      </SessionFileContext.Provider>,
    );
    const link = screen.getByRole("link", { name: path });
    expect(link.getAttribute("href")).toBe(
      `/api/v1/sessions/session-1/files?path=${encodeURIComponent(path)}`,
    );
    expect(link.getAttribute("download")).toBe("notes.txt");
  });
});
