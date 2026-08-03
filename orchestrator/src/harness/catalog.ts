/**
 * Shared harness-catalog validation (ADR 0062/0063).
 *
 * Every surface that STORES a harness/model/effort triple validates it here:
 * ProfileService (a profile's default) and AutomationService (an automation's
 * override). One rule, one set of messages — a profile save and an automation
 * save can never disagree about what a valid triple is, and a new rule
 * (deprecation, aliasing, org-scoping) lands in one place instead of drifting
 * between two services. Mirrors `skills/catalog.ts`, which backs the profile
 * editor's skill validation the same way.
 *
 * The coordinator re-checks authoritatively at session-create; failing here
 * keeps a stored record from ever naming something the editor wouldn't offer.
 */

import { Code, ConnectError } from "@connectrpc/connect";

/**
 * The slice of HarnessCatalogService this validation needs: option IDS only.
 * The compiler's richer view (`HarnessDescriptorView` in rpc/task-create.ts,
 * which carries each option's env + secrets) is assignable to this, so callers
 * pass whichever client they already hold.
 */
export interface HarnessOptionCatalog {
  listHarnesses(req: Record<string, never>): Promise<{
    harnesses: Array<{
      name: string;
      descriptor?: {
        models?: Array<{ id: string }>;
        effort?: Array<{ id: string }>;
      };
    }>;
  }>;
}

/** Optional proto strings may arrive as ""; normalize catalog selections before
 *  validation. `null` = not selected (inherit the default below it). */
export function catalogOptionId(value: string | undefined): string | null {
  return value?.trim() || null;
}

/**
 * Assert that `harness` is registered and that a set `model`/`effort` is an
 * option id on THAT harness's descriptor. A model or effort id is meaningful
 * only next to one harness, so the caller resolves which harness applies (a
 * profile always names one; an automation pins the one its ids belong to)
 * before calling.
 */
export async function assertHarnessTriple(
  catalog: HarnessOptionCatalog,
  triple: { harness: string; model?: string | null; effort?: string | null },
): Promise<void> {
  const { harness, model, effort } = triple;
  const { harnesses } = await catalog.listHarnesses({});
  const descriptor = harnesses.find((h) => h.name === harness)?.descriptor;
  if (!descriptor) {
    throw new ConnectError(`harness "${harness}" is not in the catalog`, Code.InvalidArgument);
  }
  if (model != null && !(descriptor.models ?? []).some((m) => m.id === model)) {
    throw new ConnectError(
      `model "${model}" is not valid for harness "${harness}"`,
      Code.InvalidArgument,
    );
  }
  if (effort != null && !(descriptor.effort ?? []).some((e) => e.id === effort)) {
    throw new ConnectError(
      `effort "${effort}" is not valid for harness "${harness}"`,
      Code.InvalidArgument,
    );
  }
}
