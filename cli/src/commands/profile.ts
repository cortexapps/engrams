/**
 * engrams profile … — read surface of the native ProfileService (what a
 * `task create --profile` can reference). Profile AUTHORING stays in the
 * dashboard — the shapes (icons, env, secrets) are UI-first.
 */

import type { Clients } from "../client.ts";
import { detail, failWith, printJson, table, truncate } from "../output.ts";
import type { Profile } from "../gen/engram/app/v1/profile_pb.ts";

function profileJson(p: Profile) {
  return {
    id: p.id,
    name: p.name,
    description: p.description,
    image_id: p.imageId,
    skills: p.skills,
    capabilities: p.capabilities,
    archived: p.archived,
    is_default: p.isDefault,
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
    ["capabilities", p.capabilities.join(", ")],
    ["default", p.isDefault ? "yes" : undefined],
    ["created_at", p.createdAt],
  ]);
}
