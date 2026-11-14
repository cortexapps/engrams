import { requireThat, RunnerFailure, type Context } from "./context.ts";
import { FixtureObserver, type DatabaseObserver } from "./observer.ts";
import { Supervisor } from "./supervisor.ts";
import type { PublicClient } from "./transport.ts";
import { waitFor } from "./bootstrap.ts";
import { requireKillVariants } from "./case-controls.ts";

/** Validate all fixed variants before the first mutation, not midway through kill cases. */
export function requirePrerequisites(ctx: Context) {
  if (ctx.deadlines.requestMs < 6000)
    throw new RunnerFailure(
      "NEEDS_CONTEXT",
      "guest_tls_request_deadline_6000ms",
    );
  requireKillVariants(ctx.supervisor);
  requireThat(
    ctx.supervisor.operatorCandidates.normal.identity ===
      ctx.publication.candidateIdentity,
    "normal_candidate_publication_identity",
  );
}
export async function preflight(ctx: Context, database: DatabaseObserver) {
  requirePrerequisites(ctx);
  const status = await new Supervisor(ctx).status();
  requireThat(
    status.roles.every(
      (v) => v.state === "running" && v.configurationId === "normal",
    ),
    "initial_six_roles_running_normal",
  );
  await database.prerequisites();
  for (const protocol of ["modern", "legacy"] as const) {
    const fixture = new FixtureObserver(ctx, protocol);
    await fixture.ready();
    const observed = await fixture.observe();
    requireThat(observed.held === 0, "fixture_has_no_existing_held_call");
  }
  return status;
}
export async function publicPrerequisites(ctx: Context, admin: PublicClient) {
  const images = await admin.images.listEnabledImages({});
  requireThat(images.images.length <= 128, "public_image_catalog_bound");
  const matches = images.images.filter((v) => v.id === ctx.imageId);
  requireThat(
    matches.length === 1 && matches[0]!.manifestDigest === ctx.imageDigest,
    "actual_public_enabled_image",
  );
  const response = await waitFor(
    ctx,
    "actual_enrolled_host_ready",
    () => admin.fleet.getHost({ hostId: ctx.hostId }),
    (value) => value.host?.status === "ready",
  );
  requireThat(
    response.host?.id === ctx.hostId &&
      response.host.capacityTotalMib > 0n &&
      response.host.readyImageDigests.includes(ctx.imageDigest),
    "actual_host_image_prefetched",
  );
  return {
    imageId: ctx.imageId,
    imageDigest: matches[0]!.manifestDigest,
    hostId: response.host.id,
    status: response.host.status,
  };
}
