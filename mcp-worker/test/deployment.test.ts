import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseAllDocuments } from "yaml";
import { test } from "node:test";
import { mkdtempSync, writeFileSync, rmSync, readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { validateEgressRules, RESERVED_CIDRS } from "../src/deployment.ts";
test("operator rules cannot open metadata, loopback or platform administration", () => {
  assert.throws(() =>
    validateEgressRules([{ cidr: "0.0.0.0/0", except: [] }], []),
  );
  assert.throws(() =>
    validateEgressRules([{ cidr: "fd00::/8", except: [] }], []),
  );
  assert.throws(() =>
    validateEgressRules([{ cidr: "10.0.0.0/8", except: [] }], ["10.1.0.0/16"]),
  );
  validateEgressRules(
    [{ cidr: "10.0.0.0/8", except: ["10.1.0.0/16"] }],
    ["10.1.0.0/16"],
  );
  assert.throws(() =>
    validateEgressRules([{ cidr: "169.254.169.254/32", except: [] }], []),
  );
});
test("worker chart defaults off and image pins maintained Node 24", () => {
  const chart = new URL("../../deploy/helm/engram", import.meta.url).pathname;
  const rendered = execFileSync(
    "helm",
    ["template", "test", chart, "-f", `${chart}/ci/render-values.yaml`],
    { encoding: "utf8", env: { ...process.env, HELM_PLUGINS: "/nonexistent" } },
  );
  assert.equal(rendered.includes("component: mcp-worker"), false);
  const dockerfile = readFileSync(
    new URL("../../docker/mcp-worker.Dockerfile", import.meta.url),
    "utf8",
  );
  assert.ok(dockerfile.includes("node:24.22.2-bookworm-slim"));
  assert.ok(dockerfile.includes("ulimit -c 0"));
});

test("orchestrator image includes locked worker production dependencies beside worker source", () => {
  const dockerfile = readFileSync(
    new URL("../../docker/orchestrator.Dockerfile", import.meta.url),
    "utf8",
  );
  const stages = dockerfile.split(/^FROM /m);
  const deps = stages.find((stage) =>
    stage.startsWith("node:24.22.2-bookworm-slim AS worker-deps\n"),
  );
  assert.ok(deps, "worker dependencies need a separate locked install stage");
  assert.match(deps, /^WORKDIR \/app\/mcp-worker$/m);
  assert.match(
    deps,
    /^COPY mcp-worker\/package.json mcp-worker\/package-lock.json \.\/$/m,
  );
  assert.match(deps, /^RUN npm ci --omit=dev --ignore-scripts$/m);
  const runtime = stages.at(-1)!;
  assert.match(
    runtime,
    /^COPY --from=worker-deps \/app\/mcp-worker\/node_modules \/app\/mcp-worker\/node_modules$/m,
  );
  assert.match(
    runtime,
    /^COPY --from=worker-deps \/app\/mcp-worker\/package.json \/app\/mcp-worker\/package-lock.json \/app\/mcp-worker\/$/m,
  );
  assert.match(runtime, /^COPY mcp-worker\/src\/ \/app\/mcp-worker\/src\/$/m);
  assert.match(runtime, /^USER bun$/m);
  assert.match(runtime, /^CMD \["bun", "run", "start"\]$/m);
});

test("both zone deployments have fixed identity, mounts, namespace and network limits", (t) => {
  const chart = new URL("../../deploy/helm/engram", import.meta.url).pathname;
  const dir = mkdtempSync(join(tmpdir(), "mcp-render-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const zone = {
    namespace: "worker-tools",
    isolationConfirmed: true,
    dispatchEnabled: false,
    tlsSecret: "rpc-tls",
    callers: [{ fingerprint: "fixture", role: "orchestrator" }],
    policy: {
      id: "private",
      revision: "1",
      mode: "internal",
      authorities: ["tools.example"],
      aliases: [],
      cidrs: ["10.20.0.0/16"],
      denyCidrs: ["10.20.0.0/24"],
    },
    egressRules: [{ cidr: "10.20.0.0/16", except: ["10.20.0.0/24"] }],
    ports: [443],
    dns: { namespace: "kube-system", podLabels: { "k8s-app": "kube-dns" } },
  };
  const values = {
    mcpWorker: {
      enabled: true,
      zones: [
        zone,
        {
          ...zone,
          namespace: "worker-public",
          policy: {
            ...zone.policy,
            id: "public",
            mode: "public",
            cidrs: [],
            denyCidrs: ["8.8.4.0/24"],
          },
        },
      ],
    },
  };
  const file = join(dir, "values.json");
  writeFileSync(file, JSON.stringify(values));
  const rendered = execFileSync(
    "helm",
    [
      "template",
      "test",
      chart,
      "-f",
      `${chart}/ci/render-values.yaml`,
      "-f",
      file,
    ],
    { encoding: "utf8", env: { ...process.env, HELM_PLUGINS: "/nonexistent" } },
  );
  const docs = parseAllDocuments(rendered)
    .map((d) => d.toJS())
    .filter((d) => d?.metadata?.namespace?.startsWith("worker-"));
  assert.equal(docs.length, 10);
  for (const d of docs.filter((d) => d.kind === "Deployment")) {
    const spec = d.spec.template.spec;
    assert.equal(spec.automountServiceAccountToken, false);
    assert.equal(spec.enableServiceLinks, false);
    assert.equal(spec.securityContext.runAsNonRoot, true);
    assert.equal(spec.securityContext.seccompProfile.type, "RuntimeDefault");
    assert.equal(spec.containers.length, 1);
    const c = spec.containers[0];
    assert.equal(c.securityContext.readOnlyRootFilesystem, true);
    assert.equal(c.securityContext.allowPrivilegeEscalation, false);
    assert.deepEqual(c.securityContext.capabilities.drop, ["ALL"]);
    assert.deepEqual(c.env, [{ name: "MCP_WORKER_ENABLED", value: "false" }]);
    assert.equal(spec.volumes.length, 2);
    assert.ok(spec.volumes.every((v: { hostPath?: unknown }) => !v.hostPath));
    assert.equal(spec.hostNetwork, undefined);
  }
  for (const d of docs.filter((d) => d.kind === "NetworkPolicy")) {
    assert.equal(d.spec.ingress[0].from.length, 1);
    assert.ok(d.spec.ingress[0].from[0].namespaceSelector);
    assert.ok(d.spec.ingress[0].from[0].podSelector);
    assert.equal(d.spec.egress[0].to.length, 1);
    assert.ok(d.spec.egress[0].to[0].namespaceSelector);
    assert.ok(d.spec.egress[0].to[0].podSelector);
    assert.deepEqual(d.spec.egress[1].ports, [{ protocol: "TCP", port: 443 }]);
  }
  for (const d of docs.filter((d) => d.kind === "ConfigMap")) {
    const config = JSON.parse(d.data["config.json"]);
    validateEgressRules(
      config.egressRules,
      config.zone.denyCidrs,
      config.zone.cidrs,
      config.zone.mode,
    );
  }
  assert.ok(
    docs
      .filter((d) => d.kind === "Service")
      .every((d) => d.spec.type === "ClusterIP"),
  );
  assert.ok(
    docs
      .filter((d) => d.kind === "ServiceAccount")
      .every(
        (d) =>
          d.automountServiceAccountToken === false && !d.metadata.annotations,
      ),
  );
});

test("rendered routes cannot exceed private CIDRs or admit private ranges in public zones", () => {
  assert.throws(() =>
    validateEgressRules(
      [{ cidr: "10.0.0.0/8", except: [] }],
      [],
      ["10.20.0.0/16"],
      "internal",
    ),
  );
  assert.throws(() =>
    validateEgressRules(
      [
        {
          cidr: "0.0.0.0/0",
          except: RESERVED_CIDRS.filter((c) => !c.includes(":")),
        },
      ],
      [],
      [],
      "public",
    ),
  );
});
