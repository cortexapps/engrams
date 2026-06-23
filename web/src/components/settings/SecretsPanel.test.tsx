// Contract tests for the org-secret management panel (ADR 0057 C0).
//
// These pin the RPC request shapes the panel sends to OrgSecretService via
// connect-query — PutSecret {name, value} and DeleteSecret {name} — plus the
// local validation that must fire before any RPC (a value should never be sent
// for an empty/invalid name). A custom router transport captures the exact
// proto-shaped requests; no fetch mocking required.

import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";
import { renderWithProviders } from "../../test-utils";
import { SecretsPanel } from "./SecretsPanel";
import { OrgSecretService } from "../../gen/engram/app/v1/org_secret_pb";
import type {
  OrgSecretMeta,
  PutSecretRequest,
  DeleteSecretRequest,
} from "../../gen/engram/app/v1/org_secret_pb";

function installCapturingTransport(initial: OrgSecretMeta[] = []): {
  transport: ReturnType<typeof createRouterTransport>;
  puts: PutSecretRequest[];
  deletes: DeleteSecretRequest[];
} {
  const puts: PutSecretRequest[] = [];
  const deletes: DeleteSecretRequest[] = [];
  const transport = createRouterTransport((router) => {
    router.service(OrgSecretService, {
      listSecrets: () => ({ secrets: initial }),
      putSecret: (req: PutSecretRequest) => {
        puts.push(req);
        return {
          secret: { name: req.name, keyId: "kek-test", createdAt: "", updatedAt: "" },
        };
      },
      deleteSecret: (req: DeleteSecretRequest) => {
        deletes.push(req);
        return { deleted: true };
      },
    });
  });
  return { transport, puts, deletes };
}

async function openAddDialog() {
  const user = userEvent.setup();
  const trigger = await screen.findByRole("button", { name: /add secret/i });
  await user.click(trigger);
  return user;
}

describe("SecretsPanel", () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  test("add: sends PutSecret {name, value}", async () => {
    const { transport, puts } = installCapturingTransport();
    renderWithProviders(<SecretsPanel />, { transport });

    const user = await openAddDialog();
    await user.type(screen.getByPlaceholderText("sentry-token"), "triage-db-url");
    await user.type(screen.getByPlaceholderText("•••••"), "postgres://hunter2");
    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => expect(puts.length).toBeGreaterThan(0));
    const req = puts.at(-1)!;
    expect(req.name).toBe("triage-db-url");
    expect(req.value).toBe("postgres://hunter2");
  });

  test("missing value: rejects locally, never calls PutSecret", async () => {
    const { transport, puts } = installCapturingTransport();
    renderWithProviders(<SecretsPanel />, { transport });

    const user = await openAddDialog();
    await user.type(screen.getByPlaceholderText("sentry-token"), "triage-db-url");
    // value deliberately blank
    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await screen.findByText(/value is required/i);
    expect(puts).toHaveLength(0);
  });

  test("invalid name (whitespace): rejects locally, never calls PutSecret", async () => {
    const { transport, puts } = installCapturingTransport();
    renderWithProviders(<SecretsPanel />, { transport });

    const user = await openAddDialog();
    await user.type(screen.getByPlaceholderText("sentry-token"), "bad name");
    await user.type(screen.getByPlaceholderText("•••••"), "x");
    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await screen.findByText(/letters, digits/i);
    expect(puts).toHaveLength(0);
  });

  test("remove: confirms then sends DeleteSecret {name}", async () => {
    const { transport, deletes } = installCapturingTransport([
      { name: "sentry-token", keyId: "kek-1", createdAt: "", updatedAt: "" } as OrgSecretMeta,
    ]);
    renderWithProviders(<SecretsPanel />, { transport });

    const user = userEvent.setup();
    const remove = await screen.findByRole("button", { name: /^remove$/i });
    await user.click(remove);
    // AlertDialog confirm
    await user.click(await screen.findByRole("button", { name: /remove secret/i }));

    await waitFor(() => expect(deletes.length).toBeGreaterThan(0));
    expect(deletes.at(-1)!.name).toBe("sentry-token");
  });
});
