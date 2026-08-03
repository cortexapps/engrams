import { afterEach, expect, test, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { GuardedDownload } from "./GuardedDownload";

// EVERY download goes through the trust interstitial — renderable types
// included, because the transcript offers HTML/SVG for download without
// ever rendering them, and one uniform gate beats a taxonomy of "safe
// enough" kinds.

afterEach(() => {
  vi.restoreAllMocks();
});

test("renderable types gate too — no live anchor, dialog on click", async () => {
  const user = userEvent.setup();
  render(
    <GuardedDownload url="/api/v1/artifacts/a1" mediaType="text/markdown" fileName="notes.md">
      Download
    </GuardedDownload>,
  );
  expect(screen.queryByRole("link")).toBeNull();
  await user.click(screen.getByRole("button", { name: "Download" }));
  expect(await screen.findByText("Download this file?")).toBeTruthy();
  expect(screen.getByText(/notes\.md/)).toBeTruthy();
});

test("binary downloads show the full warning copy and both actions", async () => {
  const user = userEvent.setup();
  render(
    <GuardedDownload
      url="/api/v1/artifacts/a2"
      mediaType="application/octet-stream"
      fileName="tool.bin"
      sizeBytes={2048}
    >
      Download
    </GuardedDownload>,
  );
  await user.click(screen.getByRole("button", { name: "Download" }));

  expect(await screen.findByText("Download this file?")).toBeTruthy();
  expect(screen.getByText(/tool\.bin/)).toBeTruthy();
  expect(screen.getByText(/unknown\s+sender/)).toBeTruthy();
  expect(screen.getByRole("button", { name: "Cancel" })).toBeTruthy();
  expect(screen.getByRole("button", { name: "Download anyway" })).toBeTruthy();
});

test("Download anyway forces a save even without a fileName", async () => {
  // The transient anchor must carry the bare `download` attribute — an
  // omitted attribute would NAVIGATE (renderable types serve inline),
  // replacing the SPA with the raw bytes.
  const clicked: Array<{ download: string | null; href: string }> = [];
  vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(
    function (this: HTMLAnchorElement) {
      clicked.push({
        download: this.getAttribute("download"),
        href: this.getAttribute("href") ?? "",
      });
    },
  );

  const user = userEvent.setup();
  render(
    <GuardedDownload url="/api/v1/artifacts/a3" mediaType="image/png">
      Download
    </GuardedDownload>,
  );
  await user.click(screen.getByRole("button", { name: "Download" }));
  await user.click(await screen.findByRole("button", { name: "Download anyway" }));

  expect(clicked).toEqual([{ download: "", href: "/api/v1/artifacts/a3" }]);
});
