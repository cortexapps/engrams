// Contract tests for the API-keys management panel (ADR 0086).
//
// Pin the RPC shapes the panel sends to ApiKeyService via connect-query —
// CreateApiKey {name, role, expiresAt} and RevokeApiKey {id} — plus the local
// validation that fires before any RPC, and the one-time key reveal (the
// plaintext exists only in the CreateApiKey response; the panel must surface
// it immediately or it is lost).

import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, fireEvent, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "../../test-utils";
import { ApiKeysPanel } from "./ApiKeysPanel";
import { ApiKeyService } from "../../gen/engram/app/v1/api_key_pb";
import type {
  ApiKeyMeta,
  CreateApiKeyRequest,
  RevokeApiKeyRequest,
} from "../../gen/engram/app/v1/api_key_pb";

function installCapturingTransport(initial: ApiKeyMeta[] = []): {
  transport: ReturnType<typeof createRouterTransport>;
  creates: CreateApiKeyRequest[];
  revokes: RevokeApiKeyRequest[];
} {
  const creates: CreateApiKeyRequest[] = [];
  const revokes: RevokeApiKeyRequest[] = [];
  const transport = createRouterTransport((router) => {
    router.service(ApiKeyService, {
      listApiKeys: () => ({ keys: initial }),
      createApiKey: (req: CreateApiKeyRequest) => {
        creates.push(req);
        return {
          meta: {
            id: "key-1",
            name: req.name,
            role: req.role,
            start: "engk_a1b2c3",
            createdAt: "2026-07-09T00:00:00Z",
            expiresAt: req.expiresAt,
            lastUsedAt: "",
          },
          key: "engk_plaintext-shown-once",
        };
      },
      revokeApiKey: (req: RevokeApiKeyRequest) => {
        revokes.push(req);
        return { revoked: true };
      },
    });
  });
  return { transport, creates, revokes };
}

const seeded = {
  id: "key-9",
  name: "ci-bot",
  role: "admin",
  start: "engk_zzz111",
  createdAt: "2026-07-01T00:00:00Z",
  expiresAt: "",
  lastUsedAt: "",
} as ApiKeyMeta;

async function openCreateDialog() {
  const user = userEvent.setup();
  await user.click(await screen.findByRole("button", { name: /create key/i }));
  return user;
}

describe("ApiKeysPanel", () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  test("lists keys: role badge, masked preview, Never for no expiry/use", async () => {
    const { transport } = installCapturingTransport([seeded]);
    renderWithProviders(<ApiKeysPanel />, { transport });

    expect(await screen.findByText("ci-bot")).toBeTruthy();
    expect(screen.getByText("admin")).toBeTruthy();
    expect(screen.getByText(/engk_zzz111…/)).toBeTruthy();
    expect(screen.getAllByText("Never")).toHaveLength(2);
  });

  test('create: sends {name, role, expiresAt:""} and reveals the one-time key', async () => {
    const { transport, creates } = installCapturingTransport();
    renderWithProviders(<ApiKeysPanel />, { transport });

    const user = await openCreateDialog();
    await user.type(screen.getByPlaceholderText("ci-bot"), "deploy-bot");
    await user.click(screen.getByRole("button", { name: /^create key$/i }));

    await waitFor(() => expect(creates.length).toBeGreaterThan(0));
    const req = creates.at(-1)!;
    expect(req.name).toBe("deploy-bot");
    expect(req.role).toBe("user"); // default
    expect(req.expiresAt).toBe("");

    // One-time reveal: the plaintext from the response is on screen.
    const reveal = (await screen.findByLabelText("API key")) as HTMLInputElement;
    expect(reveal.value).toBe("engk_plaintext-shown-once");
    await screen.findByText(/only time the key is shown/i);
  });

  test("create with an expiration date sends an ISO expiresAt", async () => {
    const { transport, creates } = installCapturingTransport();
    renderWithProviders(<ApiKeysPanel />, { transport });

    const user = await openCreateDialog();
    await user.type(screen.getByPlaceholderText("ci-bot"), "temp-key");
    fireEvent.change(screen.getByLabelText(/expires/i), { target: { value: "2027-01-01" } });
    await user.click(screen.getByRole("button", { name: /^create key$/i }));

    await waitFor(() => expect(creates.length).toBeGreaterThan(0));
    const req = creates.at(-1)!;
    expect(req.expiresAt).not.toBe("");
    // Parses back to the chosen day (end-of-day local).
    expect(new Date(req.expiresAt).getTime()).toBeGreaterThan(Date.parse("2026-12-31"));
  });

  test("empty name rejects locally, never calls CreateApiKey", async () => {
    const { transport, creates } = installCapturingTransport();
    renderWithProviders(<ApiKeysPanel />, { transport });

    const user = await openCreateDialog();
    await user.click(screen.getByRole("button", { name: /^create key$/i }));

    await screen.findByText(/name is required/i);
    expect(creates).toHaveLength(0);
  });

  test("past expiration rejects locally, never calls CreateApiKey", async () => {
    const { transport, creates } = installCapturingTransport();
    renderWithProviders(<ApiKeysPanel />, { transport });

    const user = await openCreateDialog();
    await user.type(screen.getByPlaceholderText("ci-bot"), "temp-key");
    fireEvent.change(screen.getByLabelText(/expires/i), { target: { value: "2020-01-01" } });
    await user.click(screen.getByRole("button", { name: /^create key$/i }));

    await screen.findByText(/at least a day out/i);
    expect(creates).toHaveLength(0);
  });

  test("revoke: confirms then sends RevokeApiKey {id}", async () => {
    const { transport, revokes } = installCapturingTransport([seeded]);
    renderWithProviders(<ApiKeysPanel />, { transport });

    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /^revoke$/i }));
    await user.click(await screen.findByRole("button", { name: /revoke key/i }));

    await waitFor(() => expect(revokes.length).toBeGreaterThan(0));
    expect(revokes.at(-1)!.id).toBe("key-9");
  });
});
