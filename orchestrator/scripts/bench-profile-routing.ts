/**
 * Benchmark a profile router (the `decide` block's routing questions) against
 * historical routing decisions.
 *
 *   OPENROUTER_API_KEY=… bun scripts/bench-profile-routing.ts \
 *     --cards cards.jsonl --samples web.jsonl,slack.jsonl --out results/ \
 *     [--model typesafe/jev-1.13] [--concurrency 8] [--limit N]
 *
 * --cards: one ProfileCard per line (the CURRENT profiles; see
 *   src/automations/profile-cards.ts). --samples: one RoutingSample per line
 *   (src/automations/routing-bench.ts); `label` is the profile id the request
 *   actually ran on. Export both from your own deployment; they hold request
 *   text, so keep them outside the repository.
 *
 * Writes results.jsonl (one decision per sample) and report.json (per-slice
 * metrics), and prints a summary. The requests are built with the same code
 * as the production graph: profileOptions + profileRouteQuestions +
 * prepareQuestions/mapAnswers.
 */

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { parseArgs } from "node:util";

import { DEFAULT_DECISION_MODEL, mapAnswers, prepareQuestions } from "../src/automations/engine/blocks/decide.ts";
import { profileOptions, profileRouteQuestions, type ProfileCard } from "../src/automations/profile-cards.ts";
import {
  sampleState,
  scorableSamples,
  scoreSlice,
  type RoutingResult,
  type RoutingSample,
} from "../src/automations/routing-bench.ts";
import { DecisionsApiError, makeDecisionsClient } from "../src/integrations/openrouter-decisions.ts";

const { values } = parseArgs({
  options: {
    cards: { type: "string" },
    samples: { type: "string" },
    out: { type: "string" },
    model: { type: "string", default: DEFAULT_DECISION_MODEL },
    concurrency: { type: "string", default: "8" },
    limit: { type: "string" },
  },
});
if (!values.cards || !values.samples || !values.out) {
  console.error("usage: --cards <jsonl> --samples <jsonl,...> --out <dir> [--model m] [--concurrency n] [--limit n]");
  process.exit(2);
}
const apiKey = process.env["OPENROUTER_API_KEY"];
if (!apiKey) {
  console.error("OPENROUTER_API_KEY is not set");
  process.exit(2);
}

function readJsonl<T>(path: string): T[] {
  return readFileSync(path, "utf8")
    .split("\n")
    .filter((line) => line.trim())
    .map((line) => JSON.parse(line) as T);
}

const cards = readJsonl<ProfileCard>(values.cards);
const all = values.samples.split(",").flatMap((p) => readJsonl<RoutingSample>(p));
let samples = scorableSamples(all, cards);
if (values.limit) samples = samples.slice(0, Number(values.limit));
console.error(`${cards.length} profiles; ${samples.length} scorable of ${all.length} samples`);

const prepared = prepareQuestions(profileRouteQuestions(profileOptions(cards)));
if (!prepared.ok) throw new Error(prepared.error);
const client = makeDecisionsClient({ apiKey, timeoutMs: 15_000 });

async function decideOnce(sample: RoutingSample): Promise<RoutingResult> {
  if (!prepared.ok) throw new Error("unreachable");
  const request = { model: values.model!, state: sampleState(sample)!, questions: prepared.value.questions };
  for (let attempt = 0; ; attempt++) {
    const started = performance.now();
    try {
      const response = await client.decide(request);
      const latencyMs = performance.now() - started;
      const mapped = mapAnswers(prepared.value, response.answers);
      if (!mapped.ok) return { sample, latencyMs, error: mapped.error };
      const profile = mapped.value["profile"] as { value: string; ranked: string[]; confidence: number };
      const wants = mapped.value["wants_choice"] as { yes: number };
      return {
        sample,
        predicted: profile.value,
        ranked: profile.ranked,
        confidence: profile.confidence,
        wantsChoice: wants.yes,
        latencyMs,
        cost: response.usage.cost ?? null,
      };
    } catch (error) {
      const retryable = error instanceof DecisionsApiError && error.retryable;
      if (!retryable || attempt >= 4) {
        return { sample, latencyMs: performance.now() - started, error: String(error) };
      }
      await Bun.sleep(500 * 2 ** attempt);
    }
  }
}

const results: RoutingResult[] = new Array(samples.length);
let next = 0;
let done = 0;
await Promise.all(
  Array.from({ length: Math.max(1, Number(values.concurrency)) }, async () => {
    while (next < samples.length) {
      const i = next++;
      results[i] = await decideOnce(samples[i]!);
      if (++done % 50 === 0) console.error(`${done}/${samples.length}`);
    }
  }),
);

mkdirSync(values.out, { recursive: true });
writeFileSync(join(values.out, "results.jsonl"), results.map((r) => JSON.stringify(r)).join("\n") + "\n");

const slices = [...new Set(results.map((r) => r.sample.slice))];
const reports = [
  ...slices.map((slice) => scoreSlice(slice, results.filter((r) => r.sample.slice === slice), all)),
  scoreSlice("all", results, all),
];
writeFileSync(join(values.out, "report.json"), JSON.stringify({ model: values.model, cards: cards.length, reports }, null, 2));

const pct = (x: number | null) => (x === null ? "  -  " : `${(x * 100).toFixed(1)}%`);
for (const r of reports) {
  console.log(
    `${r.slice.padEnd(14)} n=${String(r.n).padStart(4)} err=${r.errors} top1=${pct(r.top1)} top2=${pct(r.top2)} ` +
      `| majority=${pct(r.baselines.majority)} user-prior=${pct(r.baselines.userPrior)} channel-prior=${pct(r.baselines.channelPrior)} ` +
      `| p50=${r.latencyMs.p50.toFixed(0)}ms p95=${r.latencyMs.p95.toFixed(0)}ms $/1k=${r.cost.per1k.toFixed(4)}`,
  );
  console.log(
    "    coverage→accuracy: " +
      r.coverage.map((c) => `≥${c.threshold}: ${pct(c.coverage)}→${pct(c.accuracy)}`).join("  "),
  );
}
