// Regression tests for the AddRegistryForm payload contract.
//
// These tests verify the RPC request shape sent to ImageService.AddRegistry
// via connect-query. A refactor that ships `password_hash` instead of
// `password`, or collapses the oneof into a flat field, would slip past
// the Rust integration test. The contract sits between the two services;
// both ends need a regression pin.
//
// The tests supply a custom connect-query transport so we can capture
// the exact proto-shaped request objects the component sends — no
// mocking of `fetch` required.

import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "../../test-utils";
import { RegistriesPanel } from "./RegistriesPanel";
import { ImageService } from "../../gen/engram/app/v1/image_pb";
import type { AddRegistryRequest } from "../../gen/engram/app/v1/image_pb";

/** Build a transport that records AddRegistry calls.
 * All other RPCs fall through to the default testTransport stubs. */
function installCapturingTransport(): {
  transport: ReturnType<typeof createRouterTransport>;
  calls: AddRegistryRequest[];
} {
  const calls: AddRegistryRequest[] = [];
  const transport = createRouterTransport((router) => {
    // Capture AddRegistry calls; let everything else come from testTransport defaults.
    router.service(ImageService, {
      listRegistries: () => ({ registries: [] }),
      addRegistry: (req: AddRegistryRequest) => {
        calls.push(req);
        return {
          id: "00000000-0000-0000-0000-000000000000",
          host: req.host,
          authKind: "",
          authPrincipal: undefined,
        };
      },
      deleteRegistry: () => ({}),
      listEnabledImages: () => ({ images: [] }),
      enableImage: () => ({ job: undefined }),
      disableImage: () => ({}),
      refreshImage: () => ({ job: undefined }),
      listEnableJobs: () => ({ jobs: [] }),
      getEnableJob: () => ({ job: undefined }),
      retryEnableJob: () => ({ job: undefined }),
    });
  });
  return { transport, calls };
}

/** Open the inline AddRegistryForm. The panel renders the trigger
 * once the empty-list query has resolved, so we wait for it. */
async function openAddForm() {
  const user = userEvent.setup();
  const trigger = await screen.findByRole("button", {
    name: /register a new registry/i,
  });
  await user.click(trigger);
  return user;
}

describe("AddRegistryForm payload contract", () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  test('static auth: sends {host, auth: {case:"static", value:{username,password}}}', async () => {
    const { transport, calls } = installCapturingTransport();
    renderWithProviders(<RegistriesPanel />, { transport });

    const user = await openAddForm();

    // Static is the default auth kind, so we just fill in the three
    // visible fields and submit.
    await user.type(screen.getByPlaceholderText("ghcr.io"), "gcr.io");
    await user.type(screen.getByPlaceholderText("username or _json_key"), "_json_key");
    await user.type(screen.getByPlaceholderText("•••••"), "hunter2");

    await user.click(screen.getByRole("button", { name: /^register$/i }));

    await waitFor(() => {
      expect(calls.length).toBeGreaterThan(0);
    });
    const req = calls.at(-1)!;
    expect(req.host).toBe("gcr.io");
    expect(req.auth.case).toBe("static");
    expect(req.auth.value).toMatchObject({ username: "_json_key", password: "hunter2" });
  });

  test('gcp workload identity (ambient): sends {auth: {case:"gcpWorkloadIdentity"}} with no impersonate_sa', async () => {
    const { transport, calls } = installCapturingTransport();
    renderWithProviders(<RegistriesPanel />, { transport });

    const user = await openAddForm();
    await user.type(screen.getByPlaceholderText("ghcr.io"), "us-east1-docker.pkg.dev");

    // Click the GCP WI radio card. We match on the visible label
    // text so the test survives DOM-structure refactors.
    await user.click(screen.getByRole("radio", { name: /gcp workload identity/i }));
    // Wait for the GCP-WI form fragment to mount (AnimatePresence
    // mode="wait" waits for the static fragment's exit animation
    // first). findBy* polls until present.
    await screen.findByPlaceholderText(/engram@my-project/);
    // Leave impersonate empty — ambient identity path.

    await user.click(screen.getByRole("button", { name: /^register$/i }));

    await waitFor(() => {
      expect(calls.length).toBeGreaterThan(0);
    });
    const req = calls.at(-1)!;
    expect(req.host).toBe("us-east1-docker.pkg.dev");
    expect(req.auth.case).toBe("gcpWorkloadIdentity");
    // Ambient: impersonateSa must be absent / undefined (not empty string).
    const val = req.auth.value as { impersonateSa?: string } | undefined;
    expect(val?.impersonateSa ?? undefined).toBeUndefined();
  });

  test("gcp workload identity with impersonation: sends impersonateSa verbatim", async () => {
    const { transport, calls } = installCapturingTransport();
    renderWithProviders(<RegistriesPanel />, { transport });

    const user = await openAddForm();
    await user.type(screen.getByPlaceholderText("ghcr.io"), "us-east1-docker.pkg.dev");
    await user.click(screen.getByRole("radio", { name: /gcp workload identity/i }));
    const impersonate = await screen.findByPlaceholderText(/engram@my-project/);
    await user.type(impersonate, "engram@cortex.iam.gserviceaccount.com");

    await user.click(screen.getByRole("button", { name: /^register$/i }));

    await waitFor(() => {
      expect(calls.length).toBeGreaterThan(0);
    });
    const req = calls.at(-1)!;
    expect(req.host).toBe("us-east1-docker.pkg.dev");
    expect(req.auth.case).toBe("gcpWorkloadIdentity");
    const val = req.auth.value as { impersonateSa?: string } | undefined;
    expect(val?.impersonateSa).toBe("engram@cortex.iam.gserviceaccount.com");
  });

  test("static missing username: rejects locally, never calls AddRegistry", async () => {
    // The form must validate before RPC — surfacing inline errors
    // is friendlier than waiting for the server's 400 + a generic
    // "Bad request" toast.
    const { transport, calls } = installCapturingTransport();
    renderWithProviders(<RegistriesPanel />, { transport });

    const user = await openAddForm();
    await user.type(screen.getByPlaceholderText("ghcr.io"), "gcr.io");
    await user.type(screen.getByPlaceholderText("•••••"), "hunter2");
    // username deliberately blank.

    await user.click(screen.getByRole("button", { name: /^register$/i }));

    // Inline error rendered.
    await screen.findByText(/username is required/i);
    // No RPC fired.
    expect(calls).toHaveLength(0);
  });

  test("static missing password: rejects locally, never calls AddRegistry", async () => {
    const { transport, calls } = installCapturingTransport();
    renderWithProviders(<RegistriesPanel />, { transport });

    const user = await openAddForm();
    await user.type(screen.getByPlaceholderText("ghcr.io"), "gcr.io");
    await user.type(screen.getByPlaceholderText("username or _json_key"), "_json_key");
    // password blank.

    await user.click(screen.getByRole("button", { name: /^register$/i }));

    await screen.findByText(/password is required/i);
    expect(calls).toHaveLength(0);
  });

  test('aws ecr: sends {auth: {case:"awsEcr", value:{}}} with no assume-role field', async () => {
    const { transport, calls } = installCapturingTransport();
    renderWithProviders(<RegistriesPanel />, { transport });

    const user = await openAddForm();
    await user.type(
      screen.getByPlaceholderText("ghcr.io"),
      "123456789012.dkr.ecr.us-east-1.amazonaws.com",
    );
    await user.click(screen.getByRole("radio", { name: /aws ecr/i }));
    // Wait for the ECR fragment to mount (AnimatePresence exit-then-
    // enter, same as the GCP-WI fragment above).
    await screen.findByText(/ecr:GetAuthorizationToken/);

    await user.click(screen.getByRole("button", { name: /^register$/i }));

    await waitFor(() => {
      expect(calls.length).toBeGreaterThan(0);
    });
    const req = calls.at(-1)!;
    expect(req.host).toBe("123456789012.dkr.ecr.us-east-1.amazonaws.com");
    expect(req.auth.case).toBe("awsEcr");
    // The server rejects any assume_role_arn (not yet supported); the
    // client must send an empty payload, never the field.
    expect(req.auth.value).toMatchObject({});
    const val = req.auth.value as { assumeRoleArn?: string } | undefined;
    expect(val?.assumeRoleArn ?? undefined).toBeUndefined();
  });

  test("aws ecr: assume-role input is disabled (coming soon)", async () => {
    const { transport } = installCapturingTransport();
    renderWithProviders(<RegistriesPanel />, { transport });

    const user = await openAddForm();
    await user.click(screen.getByRole("radio", { name: /aws ecr/i }));

    const arnInput = await screen.findByPlaceholderText(/arn:aws:iam::/);
    expect((arnInput as HTMLInputElement).disabled).toBe(true);
  });

  test("aws ecr with a non-ECR host: rejects locally, never calls AddRegistry", async () => {
    const { transport, calls } = installCapturingTransport();
    renderWithProviders(<RegistriesPanel />, { transport });

    const user = await openAddForm();
    await user.type(screen.getByPlaceholderText("ghcr.io"), "ghcr.io");
    await user.click(screen.getByRole("radio", { name: /aws ecr/i }));
    await screen.findByText(/ecr:GetAuthorizationToken/);

    await user.click(screen.getByRole("button", { name: /^register$/i }));

    await screen.findByText(/needs an ECR host/i);
    expect(calls).toHaveLength(0);
  });
});
