/**
 * engrams profile … — read surface of the native ProfileService (what a
 * `task create --profile` can reference). Profile AUTHORING stays in the
 * dashboard — the shapes (icons, env, secrets) are UI-first.
 */

import type { Clients } from "../client.ts";
import { detail, failWith, printJson, table, truncate } from "../output.ts";
import type { Profile } from "../gen/engram/app/v1/profile_pb.ts";

function formatIntegrationGrant(grant: Profile["integrationGrants"][number]): string {
  const operation = `${grant.connectionId}:${grant.operation}`;
  return grant.resourceConstraints.length === 0
    ? operation
    : grant.resourceConstraints.map((resource) => `${operation}@${resource}`).join(", ");
}

function profileJson(p: Profile) {
  return {
    id: p.id,
    name: p.name,
    description: p.description,
    image_id: p.imageId,
    skills: p.skills,
    integration_grants: p.integrationGrants.map((grant) => ({
      connection_id: grant.connectionId,
      operation: grant.operation,
      resource_constraints: grant.resourceConstraints,
    })),
    // ADR 0118. Omitting these was actively misleading rather than merely
    // incomplete: a consumer writing `profile.apps ?? []` cannot tell "this
    // profile declares no apps" from "the CLI did not serialize the field", and
    // reads the second as the first. That happened — a correctly-configured
    // profile was reported as having no apps, and the next step was nearly to
    // tell the operator their change had not saved.
    apps: p.apps.map((a) => ({ name: a.name, port: a.port })),
    env_vars: p.envVars,
    archived: p.archived,
    created_at: p.createdAt,
    updated_at: p.updatedAt,
  };
}

export async function list(c: Clients, json: boolean): Promise<void> {
  const resp = await c.profile.listProfiles({}).catch(failWith);
  const rows = resp.profiles.filter((p) => !p.archived);
  if (json) {
    printJson({ profiles: rows.map(profileJson) });
    return;
  }
  if (rows.length === 0) {
    console.log("(no profiles — create one in the dashboard under Settings → Profiles)");
    return;
  }
  table(
    ["ID", "NAME", "SKILLS", "DESCRIPTION"],
    rows.map((p) => [
      p.id,
      truncate(p.name, 20),
      truncate(p.skills.join(","), 24),
      truncate(p.description, 40),
    ]),
    [36, 20, 24, 40],
  );
}

export async function get(c: Clients, id: string, json: boolean): Promise<void> {
  const resp = await c.profile.getProfile({ id }).catch(failWith);
  const p = resp.profile;
  if (!p) failWith(new Error("response carried no profile"));
  if (json) {
    printJson(profileJson(p));
    return;
  }
  detail([
    ["id", p.id],
    ["name", p.name],
    ["description", p.description],
    ["image_id", p.imageId],
    ["skills", p.skills.join(", ")],
    // ADR 0118: `<name>:<port>`, matching how the dashboard renders them.
    ["apps", p.apps.map((a) => `${a.name}:${a.port}`).join(", ")],
    // NAMES only. The values are ordinary profile config rather than secrets,
    // but they are the kind of thing an operator pastes into a shared terminal
    // or a ticket, and there is no reason to spill them for a `get`. `--json`
    // carries the full map for anyone who explicitly asked for it.
    ["env_vars", Object.keys(p.envVars).sort().join(", ")],
    ["integration_grants", p.integrationGrants.map(formatIntegrationGrant).join(", ")],
    ["created_at", p.createdAt],
  ]);
}
