import { expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { ZonePolicy } from "../../../mcp-worker/src/network.ts";
import { canonicalJson } from "../mcp/hash.ts";
import { parsePublishedOperator, makeOperatorReader } from "../mcp/operator.ts";

const hash = (value: string) => `sha256:${createHash("sha256").update(value).digest("hex")}`;
function fixture() {
  const config = {
    id: "public",
    revision: "1",
    mode: "public" as const,
    authorities: ["tools.example.test"],
    aliases: ["alias.example.test"],
    cidrs: [],
    denyCidrs: [],
  };
  const zone = {
    revision: "1",
    worker_digest: `sha256:${new ZonePolicy(config).digest}`,
    authorities: config.authorities,
    aliases: config.aliases,
    configuration: JSON.parse(canonicalJson(config)),
  };
  const expected = {
    deploymentIdentity: hash("deployment"),
    networkIdentity: hash("network"),
    trustIdentity: hash("trust"),
    caContentDigests: { control: hash("ca-content") },
  };
  const candidate = {
    version: 1,
    zones: { public: zone },
    deployment_identity: expected.deploymentIdentity,
    network_identity: expected.networkIdentity,
    trust_identity: expected.trustIdentity,
    ca_content_digests: expected.caContentDigests,
  };
  const value = {
    published: {
      generation: 1,
      expected_generation: null,
      identity: hash(JSON.stringify(candidate)),
      candidate,
    },
    pending: null,
    admission_open: true,
  };
  return { value, expected, encode: () => new TextEncoder().encode(JSON.stringify(value)) };
}
test("published operator reader binds exact complete coordinator and worker identities", () => {
  const f = fixture();
  const actual = parsePublishedOperator(f.encode(), f.expected);
  expect(actual.generation).toBe(1);
  expect(actual.policies[0]).toMatchObject({
    zoneId: "public",
    authorities: ["tools.example.test"],
    publication: { ...f.expected, generation: 1, identity: f.value.published.identity },
  });
});
test("operator reader rejects pending, incompatible, altered and absent authority", () => {
  for (const mutate of [
    (f: ReturnType<typeof fixture>) => {
      f.value.admission_open = false;
    },
    (f: ReturnType<typeof fixture>) => {
      Object.assign(f.value, { pending: f.value.published });
    },
    (f: ReturnType<typeof fixture>) => {
      f.value.published.candidate.network_identity = hash("changed");
    },
    (f: ReturnType<typeof fixture>) => {
      f.expected.caContentDigests.control = hash("changed-ca");
    },
    (f: ReturnType<typeof fixture>) => {
      f.value.published.candidate.zones.public.worker_digest = hash("fake-worker");
      f.value.published.identity = hash(JSON.stringify(f.value.published.candidate));
    },
    (f: ReturnType<typeof fixture>) => {
      f.value.published.candidate.zones.public.aliases.push("unpublished.example.test");
    },
  ]) {
    const f = fixture();
    mutate(f);
    expect(() => parsePublishedOperator(f.encode(), f.expected)).toThrow();
  }
  expect(() => parsePublishedOperator(new Uint8Array(), fixture().expected)).toThrow();
});
test("operator selection performs a new published read for every operation", async () => {
  const f = fixture();
  let calls = 0;
  const reader = makeOperatorReader(
    {
      readPublishedOperator: async () => {
        calls++;
        return { protocol: 1, publicationJson: f.encode() };
      },
    },
    f.expected,
  );
  expect((await reader.read()).policies).toHaveLength(1);
  f.value.admission_open = false;
  await expect(reader.read()).rejects.toThrow();
  expect(calls).toBe(2);
});
