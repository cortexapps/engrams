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

// Minimal proto-shaped enabled-image row for tests that need a row in the
// list. ADR 0080: the row carries the RPC-supplied ImageConfig; capture env
// rides config.warm.env, so passing entries also implies a warm command.
function makeProtoImage(
  imageUri: string,
  captureEnv: ProtoCaptureEnvEntry[] = [],
): ProtoEnabledImageSummary {
  return {
    $typeName: "engram.app.v1.EnabledImageSummary",
    id: "row-1",
    imageUri,
    manifestDigest: "sha256:abc",
    lastRefreshedAt: new Date().toISOString(),
    createdAt: new Date().toISOString(),
    config: {
      $typeName: "engram.app.v1.ImageConfig",
      name: "cortex-api",
      description: "",
      env: {},
      resources: {
        $typeName: "engram.app.v1.ImageResources",
        suggestedVcpus: 2,
      },
      warm:
        captureEnv.length > 0
          ? {
              $typeName: "engram.app.v1.ImageWarmConfig",
              command: ["/opt/engram/warm.sh"],
              env: captureEnv,
            }
          : undefined,
    },
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
  test("enable form sends {imageUri, config} to ImageService.EnableImage", async () => {
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
    await user.type(screen.getByLabelText("Name"), "cortex-api");
    await user.click(screen.getByRole("button", { name: /^enable$/i }));

    await waitFor(() => {
      expect(captures.enableCalls.length).toBeGreaterThan(0);
    });
    const req = captures.enableCalls.at(-1)!;
    expect(req.imageUri).toBe("ghcr.io/cortex/api:warm-1");
    // ADR 0080: the form is the full config and is ALWAYS sent (a first
    // enable is rejected server-side without one). vCPUs default to 2.
    expect(req.config?.name).toBe("cortex-api");
    expect(req.config?.resources?.suggestedVcpus).toBe(2);
    // Empty warm command → no [warm] hook in the config.
    expect(req.config?.warm).toBeUndefined();
  });

  test("enable form builds config.warm from the command + env editor rows", async () => {
    const { transport, captures } = installCapturingTransport([]);
    renderWithProviders(<ImagesPanel />, { transport });

    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /enable a new image/i }));

    await user.type(
      screen.getByPlaceholderText("ghcr.io/cortex/api:warm-1"),
      "ghcr.io/cortex/api:warm-1",
    );
    await user.type(screen.getByLabelText("Name"), "cortex-api");
    // ADR 0080: capture env rides the [warm] block, so it needs a command
    // (space-separated argv).
    await user.type(screen.getByLabelText(/warm command/i), "/opt/warm.sh --all");

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
    expect(req.config?.warm?.command).toEqual(["/opt/warm.sh", "--all"]);
    expect(req.config?.warm?.env).toHaveLength(1);
    expect(req.config?.warm?.env[0].name).toBe("FEATURE_FLAGS");
    expect(req.config?.warm?.env[0].value).toEqual({ case: "literal", value: "beta,fast" });
  });

  test("edit config pre-fills the form and re-submits the full config", async () => {
    // An enabled image already carrying a warm hook with a secret-ref env
    // var. The edit affordance opens the same form, pinned to the URI +
    // pre-filled from the row's config; a plain re-submit round-trips it
    // (ADR 0080: the form always sends the full config, which replaces).
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

    await user.click(screen.getByRole("button", { name: /edit config/i }));

    // Pre-filled from the row's current config.
    expect((screen.getByLabelText("Name") as HTMLInputElement).value).toBe("cortex-api");
    expect((screen.getByLabelText("vCPUs") as HTMLInputElement).value).toBe("2");
    expect((screen.getByLabelText(/warm command/i) as HTMLInputElement).value).toBe(
      "/opt/engram/warm.sh",
    );
    expect((screen.getByLabelText("Variable name") as HTMLInputElement).value).toBe(
      "OPENAI_API_KEY",
    );
    expect((screen.getByLabelText("Variable value") as HTMLInputElement).value).toBe(
      "gcp-sm://projects/p/secrets/k/versions/latest",
    );

    // Re-submit (edit path is a re-enable with the full config).
    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(captures.enableCalls.length).toBe(1);
    });
    const req = captures.enableCalls[0];
    expect(req.imageUri).toBe("ghcr.io/cortex/api:warm-1");
    expect(req.config?.name).toBe("cortex-api");
    expect(req.config?.resources?.suggestedVcpus).toBe(2);
    expect(req.config?.warm?.command).toEqual(["/opt/engram/warm.sh"]);
    expect(req.config?.warm?.env).toHaveLength(1);
    expect(req.config?.warm?.env[0].name).toBe("OPENAI_API_KEY");
    expect(req.config?.warm?.env[0].value).toEqual({
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
