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
import { cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ConnectError, Code, createRouterTransport } from "@connectrpc/connect";
import { create, equals } from "@bufbuild/protobuf";
import { renderWithProviders } from "../../test-utils";
import { ImagesPanel } from "./ImagesPanel";
import {
  EnabledImageSummarySchema,
  ImageConfigSchema,
  ImageService,
} from "../../gen/engram/app/v1/image_pb";
import type {
  EnableImageRequest,
  UpdateImageRequest,
  DisableImageRequest,
  RefreshImageRequest,
  CaptureEnvEntry as ProtoCaptureEnvEntry,
  EnabledImageSummary as ProtoEnabledImageSummary,
  EnableJob as ProtoEnableJob,
} from "../../gen/engram/app/v1/image_pb";
import { OrgSecretService } from "../../gen/engram/app/v1/org_secret_pb";

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
    // ADR 0088 UI follow-up: "[]" before any stage frame arrives.
    warmStages: "[]",
    materializeStages: "[]",
    materializeHostId: undefined,
  };
}

interface Captures {
  enableCalls: EnableImageRequest[];
  updateCalls: UpdateImageRequest[];
  disableCalls: DisableImageRequest[];
  refreshCalls: RefreshImageRequest[];
}

/** Build a transport that records image-mutation calls with controllable
 * list state. With `updateRequiresRecapture`, UpdateImage mimics the ADR
 * 0080 server contract for a capture-affecting diff: reject
 * FailedPrecondition (naming the fields) unless `allowRecapture` is set. */
function installCapturingTransport(
  initialImages: ProtoEnabledImageSummary[] = [],
  initialJobs: ProtoEnableJob[] = [],
  disableError?: unknown,
  updateRequiresRecapture = false,
): { transport: ReturnType<typeof createRouterTransport>; captures: Captures } {
  const captures: Captures = {
    enableCalls: [],
    updateCalls: [],
    disableCalls: [],
    refreshCalls: [],
  };
  const transport = createRouterTransport((router) => {
    router.service(ImageService, {
      listEnabledImages: () => ({ images: initialImages }),
      listEnableJobs: () => ({ jobs: initialJobs }),
      enableImage: (req: EnableImageRequest) => {
        captures.enableCalls.push(req);
        return { job: undefined };
      },
      updateImage: (req: UpdateImageRequest) => {
        captures.updateCalls.push(req);
        if (updateRequiresRecapture && !req.allowRecapture) {
          throw new ConnectError(
            "changing resources.suggested_vcpus requires allow_recapture",
            Code.FailedPrecondition,
          );
        }
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
    // The panel's warm-env secret-ref combobox lists org-secret names.
    router.service(OrgSecretService, {
      listSecrets: () => ({ secrets: [] }),
      putSecret: () => ({ secret: undefined }),
      deleteSecret: () => ({}),
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

  test("cheap edit (rename) goes through UpdateImage without recapture confirmation", async () => {
    // ADR 0080 phase 2b: the edit path is UpdateImage, sent optimistically
    // with allowRecapture=false. A cheap diff (name) applies immediately —
    // no confirm UI, no EnableImage fallback, dialog closes.
    const { transport, captures } = installCapturingTransport([
      makeProtoImage("ghcr.io/cortex/api:warm-1"),
    ]);
    renderWithProviders(<ImagesPanel />, { transport });

    const user = userEvent.setup();
    await screen.findByText("ghcr.io/cortex/api:warm-1");
    await user.click(screen.getByRole("button", { name: /edit config/i }));

    const name = screen.getByLabelText("Name") as HTMLInputElement;
    expect(name.value).toBe("cortex-api");
    await user.clear(name);
    await user.type(name, "renamed-api");
    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(captures.updateCalls.length).toBe(1);
    });
    const req = captures.updateCalls[0];
    expect(req.imageUri).toBe("ghcr.io/cortex/api:warm-1");
    expect(req.allowRecapture).toBe(false);
    expect(req.config?.name).toBe("renamed-api");
    // Edit never goes through EnableImage anymore.
    expect(captures.enableCalls).toHaveLength(0);
    // No recapture confirmation appeared, and the dialog closed.
    expect(screen.queryByText("This edit changes capture-affecting fields")).toBeNull();
    await waitFor(() => {
      expect(screen.queryByLabelText("Name")).toBeNull();
    });
  });

  test("capture-affecting edit confirms before sending with allowRecapture=true", async () => {
    // A resources/warm diff is detected locally. The dialog confirms before
    // any request, then "Recapture and apply" sends the config once with
    // allowRecapture=true (spawning the recapture job).
    const { transport, captures } = installCapturingTransport(
      [makeProtoImage("ghcr.io/cortex/api:warm-1")],
      [],
    );
    renderWithProviders(<ImagesPanel />, { transport });

    const user = userEvent.setup();
    await screen.findByText("ghcr.io/cortex/api:warm-1");
    await user.click(screen.getByRole("button", { name: /edit config/i }));

    const vcpus = screen.getByLabelText("vCPUs") as HTMLInputElement;
    await user.clear(vcpus);
    await user.type(vcpus, "4");
    await user.click(screen.getByRole("button", { name: /^save$/i }));

    // The confirm block appears before any update request is sent.
    await screen.findByText("This edit changes capture-affecting fields");
    expect(screen.getByText(/This edit changes resources —/)).toBeTruthy();
    expect(captures.updateCalls).toHaveLength(0);

    await user.click(screen.getByRole("button", { name: /recapture and apply/i }));

    await waitFor(() => {
      expect(captures.updateCalls.length).toBe(1);
    });
    expect(captures.updateCalls[0].allowRecapture).toBe(true);
    expect(captures.updateCalls[0].config?.resources?.suggestedVcpus).toBe(4);
  });

  test("server FailedPrecondition still opens confirmation for an unanticipated diff", async () => {
    const { transport, captures } = installCapturingTransport(
      [makeProtoImage("ghcr.io/cortex/api:warm-1")],
      [],
      undefined,
      /* updateRequiresRecapture */ true,
    );
    renderWithProviders(<ImagesPanel />, { transport });

    const user = userEvent.setup();
    await screen.findByText("ghcr.io/cortex/api:warm-1");
    await user.click(screen.getByRole("button", { name: /edit config/i }));
    const name = screen.getByLabelText("Name");
    await user.clear(name);
    await user.type(name, "renamed-api");
    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await screen.findByText("This edit changes capture-affecting fields");
    expect(screen.getByText(/resources\.suggested_vcpus/)).toBeTruthy();
    expect(captures.updateCalls).toHaveLength(1);
    expect(captures.updateCalls[0].allowRecapture).toBe(false);
  });

  test("untouched edit round-trips the row's config exactly (server sees a no-op)", async () => {
    // THE correctness property of the edit form (ADR 0080): pre-fill from
    // the row, submit untouched → the UpdateImageRequest config deep-equals
    // the row's config. Any drift (an absent description becoming "", a
    // dropped bigint timeout, a defaulted map) would make every innocent
    // rename read as a capture-affecting diff server-side. The fixture sets
    // EVERY config field except description — which stays ABSENT to pin the
    // absent-stays-absent half of the invariant.
    const rowConfig = create(ImageConfigSchema, {
      name: "cortex-api",
      env: { RUST_LOG: "info", CARGO_HOME: "/cache/cargo" },
      workdir: "/workspace",
      resources: {
        suggestedVcpus: 4,
        suggestedMemoryMib: 4096,
        suggestedDiskGib: 32,
      },
      warm: {
        command: ["/opt/engram/warm.sh", "--all"],
        timeoutSecs: 900n,
        workdir: "/srv",
        env: [
          { name: "FEATURE_FLAGS", value: { case: "literal", value: "beta,fast" } },
          { name: "OPENAI_API_KEY", value: { case: "secretRef", value: "openai-key" } },
        ],
        network: {
          default: "deny",
          allowHosts: ["registry.npmjs.org", "proxy.golang.org"],
          allowHostPatterns: ["*.pypi.org"],
        },
      },
    });
    const image = create(EnabledImageSummarySchema, {
      id: "row-1",
      imageUri: "ghcr.io/cortex/api:warm-1",
      manifestDigest: "sha256:abc",
      lastRefreshedAt: new Date().toISOString(),
      createdAt: new Date().toISOString(),
      config: rowConfig,
    });
    const { transport, captures } = installCapturingTransport([image]);
    renderWithProviders(<ImagesPanel />, { transport });

    const user = userEvent.setup();
    await screen.findByText("ghcr.io/cortex/api:warm-1");
    await user.click(screen.getByRole("button", { name: /edit config/i }));

    // Spot-check the pre-fill (the secret-ref value renders in the
    // org-secret combobox trigger, not an input — scope to the dialog since
    // the table's capture-env cell shows the same ref).
    expect((screen.getByLabelText("Name") as HTMLInputElement).value).toBe("cortex-api");
    expect((screen.getByLabelText("vCPUs") as HTMLInputElement).value).toBe("4");
    expect((screen.getByLabelText(/warm command/i) as HTMLInputElement).value).toBe(
      "/opt/engram/warm.sh --all",
    );
    expect(within(screen.getByRole("dialog")).getByText("openai-key")).toBeTruthy();

    // Submit with zero edits.
    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => {
      expect(captures.updateCalls.length).toBe(1);
    });
    const req = captures.updateCalls[0];
    expect(req.imageUri).toBe("ghcr.io/cortex/api:warm-1");
    expect(req.allowRecapture).toBe(false);
    // Proto-semantic deep equality — bigint timeout, env map, oneofs,
    // repeated fields, and ABSENT optionals (description) all included.
    expect(req.config).toBeDefined();
    expect(equals(ImageConfigSchema, req.config!, rowConfig)).toBe(true);
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

  test("materialize stage timeline + live output render on an active job", async () => {
    // ADR 0088 UI follow-up: the enable row must show the per-stage
    // timeline (with the open stage marked live), the substage in the
    // state badge, and the collapsible live output tail — the fix for
    // "materializing chunks" being the only thing to watch for an hour.
    const job = makeProtoJob("ghcr.io/cortex/api:warm-1", "materializing");
    job.materializeStages = JSON.stringify([
      {
        name: "pull",
        started_at: "2026-07-11T10:00:00Z",
        ended_at: "2026-07-11T10:02:46Z",
        outcome: "done",
      },
      { name: "flatten", started_at: "2026-07-11T10:02:46Z", ended_at: null, outcome: "running" },
    ]);
    job.outputTail = "materialize[flatten] 36 layers, 5706951764 compressed bytes";
    job.materializeHostId = "5b819b12-988c-b030-ecd2-45deb5bbd8ea";
    const { transport } = installCapturingTransport(
      [makeProtoImage("ghcr.io/cortex/api:warm-1")],
      [job],
    );
    renderWithProviders(<ImagesPanel />, { transport });

    await waitFor(() => {
      // Substage in the state badge + the timeline chips.
      expect(screen.getByText(/materializing chunks · flatten/i)).toBeTruthy();
      expect(screen.getByText("pull")).toBeTruthy();
      expect(screen.getByText("2m 46s")).toBeTruthy();
      // Materialize host attribution (short uuid).
      expect(screen.getByText(/host 5b819b12/i)).toBeTruthy();
      // The live tail rides a collapsed <details>.
      expect(screen.getByText(/live output/i)).toBeTruthy();
      expect(screen.getByText(/materialize\[flatten\] 36 layers/i)).toBeTruthy();
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
