// Preconditions for the e2e characterization net (ADR 0039 "Preparation").
// 1. web dev server up  2. control plane healthy  3. a no-harness demo
// image enabled. We deliberately do NOT auto-bake one (multi-minute build).
const WEB = "http://localhost:5173";
const COORD = process.env.ENGRAM_COORDINATOR_URL ?? "http://127.0.0.1:8090";

async function mustFetch(url: string, fixit: string): Promise<Response> {
  let res: Response;
  try {
    res = await fetch(url);
  } catch {
    throw new Error(`${url} not reachable — ${fixit}`);
  }
  if (!res.ok) throw new Error(`${url} returned ${res.status} — ${fixit}`);
  return res;
}

export default async function globalSetup() {
  await mustFetch(`${WEB}/`, "run `just dev` first");
  await mustFetch(
    `${COORD}/healthz`,
    "coordinator down/unhealthy — check Tilt (http://localhost:10350)",
  );
  const res = await mustFetch(`${WEB}/api/v1/enabled-images`, "run `just dev` first");
  // Shape: ListEnabledImagesResponse (web/src/types.ts:498) — { images: [...] }
  const body = (await res.json()) as {
    images?: { image_uri: string; harness_name: string | null }[];
  };
  if (!body.images?.some((i) => i.harness_name === null)) {
    throw new Error("no NO-HARNESS image enabled — run `just integration-session` once");
  }
}
