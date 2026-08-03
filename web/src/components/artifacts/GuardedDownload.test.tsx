import { expect, test } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { GuardedDownload } from "./GuardedDownload";

// Renderable kinds download directly; opaque binaries get the trust
// interstitial first — the whole point of the gate.

test("renderable types pass straight through as a download anchor", () => {
  render(
    <GuardedDownload url="/api/v1/artifacts/a1" mediaType="text/markdown" fileName="notes.md">
      Download
    </GuardedDownload>,
  );
  const anchor = screen.getByRole("link", { name: "Download" });
  expect(anchor.getAttribute("href")).toBe("/api/v1/artifacts/a1");
  expect(anchor.getAttribute("download")).toBe("notes.md");
});

test("a missing fileName still forces a download, never inline navigation", () => {
  // Renderable types serve Content-Disposition: inline — without the
  // bare `download` attribute the click would replace the SPA with the
  // raw bytes (review finding on the fileName-less share-card path).
  render(
    <GuardedDownload url="/api/v1/artifacts/a1" mediaType="image/png">
      Download
    </GuardedDownload>,
  );
  expect(screen.getByRole("link", { name: "Download" }).getAttribute("download")).toBe("");
});

test("binary downloads open the trust dialog instead of downloading", async () => {
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
  // The trigger is a button, not a live download link.
  expect(screen.queryByRole("link")).toBeNull();
  await user.click(screen.getByRole("button", { name: "Download" }));

  expect(await screen.findByText("Download this file?")).toBeTruthy();
  expect(screen.getByText(/tool\.bin/)).toBeTruthy();
  expect(screen.getByText(/unknown\s+sender/)).toBeTruthy();
  expect(screen.getByRole("button", { name: "Cancel" })).toBeTruthy();
  expect(screen.getByRole("button", { name: "Download anyway" })).toBeTruthy();
});
