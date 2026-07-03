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
import { ConnectError, Code, createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "../../test-utils";
import { ImagesPanel } from "./ImagesPanel";
import { ImageService } from "../../gen/engram/app/v1/image_pb";
import type {
  EnableImageRequest,
  DisableImageRequest,
  RefreshImageRequest,
  CaptureEnvEntry as ProtoCaptureEnvEntry,
  EnabledImageSummary as ProtoEnabledImageSummary,
  EnableJob as ProtoEnableJob,
} from "../../gen/engram/app/v1/image_pb";

// A proto-shaped capture-env entry (the oneof flattened to the case the
// component reads/writes).
function makeCaptureEnv(
  name: string,
  kind: "literal" | "secretRef",
  value: string,
): ProtoCaptureEnvEntry {
  return {
    $typeName: "engram.app.v1.CaptureEnvEntry",
    name,
    value: { case: kind, value },
  };
}

// Minimal proto-shaped enabled-image row for tests that need a row in the list.
function makeProtoImage(
  imageUri: string,
  captureEnv: ProtoCaptureEnvEntry[] = [],
): ProtoEnabledImageSummary {
  return {
    $typeName: "engram.app.v1.EnabledImageSummary",
    id: "row-1",
    imageUri,
    manifestDigest: "sha256:abc",
    manifestName: "cortex-api",
    manifestDescription: "",
    lastRefreshedAt: new Date().toISOString(),
    createdAt: new Date().toISOString(),
    captureEnv,
  };
}

function makeProtoJob(
  imageUri: string,
  state = "materializing",
  prestageHosts = "{}",
): ProtoEnableJob {
  return {
    $typeName: "engram.app.v1.EnableJob",
    id: "job-1",
    imageUri,
    manifestDigest: undefined,
    state,
    chunksDone: 3,
    chunksTotal: 10,
    attempts: 0,
    error: undefined,
    createdAt: new Date().toISOString(),
    updatedAt: new Date().toISOString(),
    // ADR 0036 amendment (issue #538): "{}" until the prestage stage runs.
    prestageHosts,
  };
}

interface Captures {
  enableCalls: EnableImageRequest[];
  disableCalls: DisableImageRequest[];
  refreshCalls: RefreshImageRequest[];
}

/** Build a transport that records image-mutation calls with controllable list state. */
function installCapturingTransport(
  initialImages: ProtoEnabledImageSummary[] = [],
  initialJobs: ProtoEnableJob[] = [],
  disableError?: unknown,
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
        if (disableError) throw disableError;
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
    // No rows added → an empty capture_env (server-side this inherits the
    // existing set rather than wiping it).
    expect(captures.enableCalls.at(-1)!.captureEnv).toEqual([]);
  });

  test("enable form builds capture_env entries from the editor rows", async () => {
    const { transport, captures } = installCapturingTransport([]);
    renderWithProviders(<ImagesPanel />, { transport });

    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /enable a new image/i }));

    await user.type(
      screen.getByPlaceholderText("ghcr.io/cortex/api:warm-1"),
      "ghcr.io/cortex/api:warm-1",
    );

    // Add a literal capture var (the default type). A row with an empty name
    // is skipped, so only the named row should reach the request.
    await user.click(screen.getByRole("button", { name: /add variable/i }));
    await user.type(screen.getByLabelText("Variable name"), "FEATURE_FLAGS");
    await user.type(screen.getByLabelText("Variable value"), "beta,fast");

    await user.click(screen.getByRole("button", { name: /^enable$/i }));

    await waitFor(() => {
      expect(captures.enableCalls.length).toBeGreaterThan(0);
    });
    const req = captures.enableCalls.at(-1)!;
    expect(req.imageUri).toBe("ghcr.io/cortex/api:warm-1");
    expect(req.captureEnv).toHaveLength(1);
    expect(req.captureEnv[0].name).toBe("FEATURE_FLAGS");
    expect(req.captureEnv[0].value).toEqual({ case: "literal", value: "beta,fast" });
  });

  test("edit capture env pre-fills the form and re-submits the full list", async () => {
    // An enabled image already carrying a secret-ref capture var. The edit
    // affordance opens the same form, pinned to the URI + pre-filled.
    const { transport, captures } = installCapturingTransport([
      makeProtoImage("ghcr.io/cortex/api:warm-1", [
        makeCaptureEnv(
          "OPENAI_API_KEY",
          "secretRef",
          "gcp-sm://projects/p/secrets/k/versions/latest",
        ),
      ]),
    ]);
    renderWithProviders(<ImagesPanel />, { transport });

    const user = userEvent.setup();
    await screen.findByText("ghcr.io/cortex/api:warm-1");

    await user.click(screen.getByRole("button", { name: /edit capture env/i }));

    // Pre-filled from the row's current capture_env.
    expect((screen.getByLabelText("Variable name") as HTMLInputElement).value).toBe(
      "OPENAI_API_KEY",
    );
    expect((screen.getByLabelText("Variable value") as HTMLInputElement).value).toBe(
      "gcp-sm://projects/p/secrets/k/versions/latest",
    );

    // Re-submit (edit path is a re-enable with the full list).
    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(captures.enableCalls.length).toBe(1);
    });
    const req = captures.enableCalls[0];
    expect(req.imageUri).toBe("ghcr.io/cortex/api:warm-1");
    expect(req.captureEnv).toHaveLength(1);
    expect(req.captureEnv[0].name).toBe("OPENAI_API_KEY");
    expect(req.captureEnv[0].value).toEqual({
      case: "secretRef",
      value: "gcp-sm://projects/p/secrets/k/versions/latest",
    });
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

  test("prestaging job renders the fleet chunk-prestage label", async () => {
    // ADR 0036 amendment (issue #538): the enable pipeline gained a new
    // non-terminal state between "capturing" and "ready". The dashboard
    // must render it with its own label, not fall through to "unknown".
    const { transport } = installCapturingTransport(
      [makeProtoImage("ghcr.io/cortex/api:warm-1")],
      [makeProtoJob("ghcr.io/cortex/api:warm-1", "prestaging")],
    );
    renderWithProviders(<ImagesPanel />, { transport });

    await waitFor(() => {
      expect(screen.getByText(/staging chunks to hosts/i)).toBeTruthy();
    });
  });

  test("prestage_hosts renders the per-host staged/eligible count on the dashboard", async () => {
    // review finding 5 (PR #565): prestage_hosts was plumbed through
    // proto → legacy → types.ts but never rendered — the PR body's "surfaced
    // on both the dashboard and CLI" claim was false. This pins the fix.
    const prestageHosts = JSON.stringify({
      "host-1": { outcome: "staged", waited_ms: 1200 },
      "host-2": { outcome: "timed_out", waited_ms: 20000 },
      "host-3": { outcome: "unschedulable" },
    });
    const { transport } = installCapturingTransport(
      [makeProtoImage("ghcr.io/cortex/api:warm-1")],
      [makeProtoJob("ghcr.io/cortex/api:warm-1", "prestaging", prestageHosts)],
    );
    renderWithProviders(<ImagesPanel />, { transport });

    await waitFor(() => {
      expect(screen.getByText(/1\/2 hosts staged \(1 unschedulable\)/i)).toBeTruthy();
    });
  });

  test("disable rejection surfaces the failed_precondition message inline", async () => {
    const { transport } = installCapturingTransport(
      [makeProtoImage("ghcr.io/cortex/api:warm-1")],
      [],
      new ConnectError(
        "Can't disable — 1 profile uses this image: Backend",
        Code.FailedPrecondition,
      ),
    );
    renderWithProviders(<ImagesPanel />, { transport });

    const user = userEvent.setup();
    await screen.findByText("ghcr.io/cortex/api:warm-1");
    await user.click(screen.getByRole("button", { name: /^disable$/i }));
    await user.click(await screen.findByRole("button", { name: /disable image/i }));

    expect(await screen.findByText(/1 profile uses this image/i)).toBeTruthy();
  });
});
