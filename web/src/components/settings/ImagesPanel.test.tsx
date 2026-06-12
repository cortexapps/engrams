// Regression tests for the ImagesPanel RPC contract.
//
// These tests verify the proto request shapes sent to ImageService via
// connect-query. A refactor that ships `uri` instead of `imageUri`, or
// routes disable as a DeleteImage instead of DisableImage, would slip
// past the Rust integration test. The contract sits between the two
// services; both ends need a regression pin.
//
// The tests supply a custom connect-query transport so we can capture
// the exact proto-shaped request objects the component sends.

import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "../../test-utils";
import { ImagesPanel } from "./ImagesPanel";
import { ImageService } from "../../gen/engram/app/v1/image_pb";
import type {
  EnableImageRequest,
  DisableImageRequest,
  RefreshImageRequest,
  EnabledImageSummary as ProtoEnabledImageSummary,
  EnableJob as ProtoEnableJob,
} from "../../gen/engram/app/v1/image_pb";

// Minimal proto-shaped enabled-image row for tests that need a row in the list.
function makeProtoImage(imageUri: string): Partial<ProtoEnabledImageSummary> {
  return {
    id: "row-1",
    imageUri,
    manifestDigest: "sha256:abc",
    manifestName: "cortex-api",
    manifestDescription: "",
    harnessName: "",
    lastRefreshedAt: new Date().toISOString(),
    createdAt: new Date().toISOString(),
  };
}

function makeProtoJob(imageUri: string, state = "materializing"): Partial<ProtoEnableJob> {
  return {
    id: "job-1",
    imageUri,
    state,
    chunksDone: 3,
    chunksTotal: 10,
    error: "",
    createdAt: new Date().toISOString(),
    updatedAt: new Date().toISOString(),
  };
}

interface Captures {
  enableCalls: EnableImageRequest[];
  disableCalls: DisableImageRequest[];
  refreshCalls: RefreshImageRequest[];
}

/** Build a transport that records image-mutation calls with controllable list state. */
function installCapturingTransport(
  initialImages: Partial<ProtoEnabledImageSummary>[] = [],
  initialJobs: Partial<ProtoEnableJob>[] = [],
): { transport: ReturnType<typeof createRouterTransport>; captures: Captures } {
  const captures: Captures = { enableCalls: [], disableCalls: [], refreshCalls: [] };
  const transport = createRouterTransport((router) => {
    router.service(ImageService, {
      listEnabledImages: () => ({ images: initialImages }),
      listEnableJobs: () => ({ jobs: initialJobs }),
      enableImage: (req: EnableImageRequest) => {
        captures.enableCalls.push(req);
        return { job: undefined };
      },
      disableImage: (req: DisableImageRequest) => {
        captures.disableCalls.push(req);
        return {};
      },
      refreshImage: (req: RefreshImageRequest) => {
        captures.refreshCalls.push(req);
        return { job: undefined };
      },
      getEnableJob: () => ({ job: undefined }),
      retryEnableJob: () => ({ job: undefined }),
      listRegistries: () => ({ registries: [] }),
      addRegistry: () => ({ id: "", host: "", authKind: "", authPrincipal: undefined }),
      deleteRegistry: () => ({}),
    });
  });
  return { transport, captures };
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("ImagesPanel RPC contract", () => {
  test("enable form sends {imageUri} to ImageService.EnableImage", async () => {
    const { transport, captures } = installCapturingTransport([]);
    renderWithProviders(<ImagesPanel />, { transport });

    const user = userEvent.setup();
    const trigger = await screen.findByRole("button", {
      name: /enable a new image/i,
    });
    await user.click(trigger);

    await user.type(
      screen.getByPlaceholderText("ghcr.io/cortex/api:warm-1"),
      "ghcr.io/cortex/api:warm-1",
    );
    await user.click(screen.getByRole("button", { name: /^enable$/i }));

    await waitFor(() => {
      expect(captures.enableCalls.length).toBeGreaterThan(0);
    });
    expect(captures.enableCalls.at(-1)!.imageUri).toBe("ghcr.io/cortex/api:warm-1");
  });

  test("disable button sends {imageUri} to ImageService.DisableImage", async () => {
    // The image URI contains slashes + colons, which makes it awkward as a
    // path segment. Lock that the RPC carries it as a field, not URL segment.
    const { transport, captures } = installCapturingTransport([
      makeProtoImage("ghcr.io/cortex/api:warm-1"),
    ]);
    renderWithProviders(<ImagesPanel />, { transport });

    const user = userEvent.setup();
    // Wait for the row to render.
    await screen.findByText("ghcr.io/cortex/api:warm-1");

    await user.click(screen.getByRole("button", { name: /^disable$/i }));
    // Confirmation prompt — the confirm action is labelled "Disable image"
    // to disambiguate it from the row's "Disable" trigger.
    await user.click(await screen.findByRole("button", { name: /disable image/i }));

    await waitFor(() => {
      expect(captures.disableCalls.length).toBe(1);
    });
    expect(captures.disableCalls[0].imageUri).toBe("ghcr.io/cortex/api:warm-1");
  });

  test("refresh button sends {imageUri} to ImageService.RefreshImage", async () => {
    const { transport, captures } = installCapturingTransport([
      makeProtoImage("ghcr.io/cortex/api:warm-1"),
    ]);
    renderWithProviders(<ImagesPanel />, { transport });

    const user = userEvent.setup();
    await screen.findByText("ghcr.io/cortex/api:warm-1");
    await user.click(screen.getByRole("button", { name: /^refresh$/i }));

    await waitFor(() => {
      expect(captures.refreshCalls.length).toBe(1);
    });
    expect(captures.refreshCalls[0].imageUri).toBe("ghcr.io/cortex/api:warm-1");
  });

  test("active refresh job suppresses the static image row for the same URI", async () => {
    // Both an enabled-images row and an active enable-job exist for the
    // same URI (the state right after clicking "refresh"). The panel must
    // show only the progress row — not both.
    const { transport } = installCapturingTransport(
      [makeProtoImage("ghcr.io/cortex/api:warm-1")],
      [makeProtoJob("ghcr.io/cortex/api:warm-1", "materializing")],
    );
    renderWithProviders(<ImagesPanel />, { transport });

    // The URI should appear exactly once — from the EnableJobRow.
    await waitFor(() => {
      const matches = screen.getAllByText("ghcr.io/cortex/api:warm-1");
      expect(matches).toHaveLength(1);
    });
  });
});
