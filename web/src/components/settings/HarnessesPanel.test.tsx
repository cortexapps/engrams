// Contract tests for the admin Harnesses tab (ADR 0062/0063).
//
// These pin what the panel renders from a HarnessSummary (built-in vs custom,
// the descriptor's models/effort, its required org credential cross-referenced
// against the org-secret store) and the RPC request shapes it sends:
//   - OrgSecretService.PutSecret {name: org_env, value}  (set the API key)
//   - HarnessCatalogService.RegisterHarness {name, ociRef, owner}
//   - HarnessCatalogService.DeleteHarness {name}          (custom only)
// A router transport captures the proto-shaped requests; no fetch mocking.

import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRouterTransport } from "@connectrpc/connect";
import { create } from "@bufbuild/protobuf";
import { renderWithProviders } from "../../test-utils";
import { HarnessesPanel } from "./HarnessesPanel";
import { HarnessCatalogService, HarnessSummarySchema } from "../../gen/engram/app/v1/harness_pb";
import type {
  HarnessSummary,
  RegisterHarnessRequest,
  DeleteHarnessRequest,
} from "../../gen/engram/app/v1/harness_pb";
import { OrgSecretService } from "../../gen/engram/app/v1/org_secret_pb";
import type { OrgSecretMeta, PutSecretRequest } from "../../gen/engram/app/v1/org_secret_pb";

const CLAUDE: HarnessSummary = create(HarnessSummarySchema, {
  name: "claude",
  builtIn: true,
  descriptor: {
    name: "claude",
    label: "Claude Code",
    description: "Anthropic's Claude Code agent.",
    auth: { orgEnv: "ANTHROPIC_API_KEY", userEnv: "CLAUDE_CODE_OAUTH_TOKEN" },
    models: [
      { id: "opus", label: "Claude Opus 4.8" },
      { id: "sonnet", label: "Claude Sonnet 4.6" },
    ],
    effort: [{ id: "medium", label: "Medium" }],
  },
});

const CUSTOM: HarnessSummary = create(HarnessSummarySchema, {
  name: "opencode",
  builtIn: false,
  descriptor: {
    name: "opencode",
    label: "OpenCode",
    auth: { orgEnv: "OPENCODE_API_KEY" },
    models: [],
    effort: [],
  },
});

function installTransport(
  harnesses: HarnessSummary[],
  secrets: OrgSecretMeta[] = [],
): {
  transport: ReturnType<typeof createRouterTransport>;
  puts: PutSecretRequest[];
  registers: RegisterHarnessRequest[];
  deletes: DeleteHarnessRequest[];
} {
  const puts: PutSecretRequest[] = [];
  const registers: RegisterHarnessRequest[] = [];
  const deletes: DeleteHarnessRequest[] = [];
  const transport = createRouterTransport((router) => {
    router.service(HarnessCatalogService, {
      listHarnesses: () => ({ harnesses }),
      registerHarness: (req: RegisterHarnessRequest) => {
        registers.push(req);
        return { harness: CUSTOM };
      },
      deleteHarness: (req: DeleteHarnessRequest) => {
        deletes.push(req);
        return { deleted: true };
      },
    });
    router.service(OrgSecretService, {
      listSecrets: () => ({ secrets }),
      putSecret: (req: PutSecretRequest) => {
        puts.push(req);
        return { secret: { name: req.name, keyId: "kek-test", createdAt: "", updatedAt: "" } };
      },
    });
  });
  return { transport, puts, registers, deletes };
}

describe("HarnessesPanel", () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  test("renders the built-in claude: label, models, required org credential 'not set', no delete", async () => {
    const { transport } = installTransport([CLAUDE]);
    renderWithProviders(<HarnessesPanel />, { transport });

    await screen.findByText("Claude Code");
    // getByText throws if absent — presence is the assertion.
    screen.getByText(/Claude Opus 4\.8/);
    // org credential surfaced + flagged unconfigured
    screen.getByText("ANTHROPIC_API_KEY");
    screen.getByText(/not set/i);
    // built-ins are not deletable
    expect(screen.queryByRole("button", { name: /^remove$/i })).toBeNull();
  });

  test("org credential shows 'configured' when the org secret exists", async () => {
    const { transport } = installTransport(
      [CLAUDE],
      [
        {
          name: "ANTHROPIC_API_KEY",
          keyId: "kek-1",
          createdAt: "",
          updatedAt: "",
        } as OrgSecretMeta,
      ],
    );
    renderWithProviders(<HarnessesPanel />, { transport });

    await screen.findByText("Claude Code");
    await screen.findByText(/configured/i);
    expect(screen.queryByText(/not set/i)).toBeNull();
  });

  test("set org credential: sends PutSecret {name: org_env, value}", async () => {
    const { transport, puts } = installTransport([CLAUDE]);
    renderWithProviders(<HarnessesPanel />, { transport });

    const user = userEvent.setup();
    await screen.findByText("Claude Code");
    await user.click(screen.getByRole("button", { name: /^set$/i }));
    await user.type(await screen.findByPlaceholderText("•••••"), "sk-ant-xxx");
    await user.click(screen.getByRole("button", { name: /^save$/i }));

    await waitFor(() => expect(puts.length).toBeGreaterThan(0));
    expect(puts.at(-1)!.name).toBe("ANTHROPIC_API_KEY");
    expect(puts.at(-1)!.value).toBe("sk-ant-xxx");
  });

  test("register: sends RegisterHarness {name, ociRef}", async () => {
    const { transport, registers } = installTransport([CLAUDE]);
    renderWithProviders(<HarnessesPanel />, { transport });

    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /register harness/i }));
    await user.type(await screen.findByPlaceholderText("opencode"), "opencode");
    await user.type(screen.getByPlaceholderText(/ghcr\.io/), "ghcr.io/acme/opencode-harness:v1");
    await user.click(screen.getByRole("button", { name: /^register$/i }));

    await waitFor(() => expect(registers.length).toBeGreaterThan(0));
    expect(registers.at(-1)!.name).toBe("opencode");
    expect(registers.at(-1)!.ociRef).toBe("ghcr.io/acme/opencode-harness:v1");
  });

  test("remove (custom only): confirms then sends DeleteHarness {name}", async () => {
    const { transport, deletes } = installTransport([CLAUDE, CUSTOM]);
    renderWithProviders(<HarnessesPanel />, { transport });

    const user = userEvent.setup();
    // Only the custom harness card carries a Remove button.
    const removeBtn = await screen.findByRole("button", { name: /^remove$/i });
    await user.click(removeBtn);
    await user.click(await screen.findByRole("button", { name: /remove harness/i }));

    await waitFor(() => expect(deletes.length).toBeGreaterThan(0));
    expect(deletes.at(-1)!.name).toBe("opencode");
  });
});
