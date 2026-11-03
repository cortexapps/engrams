import { join } from "node:path";
import { ZonePolicy } from "../../../../mcp-worker/src/network.ts";
import { validateEgressRules } from "../../../../mcp-worker/src/deployment.ts";
import { canonicalJson } from "../../../../orchestrator/src/mcp/hash.ts";
import { parsePublishedOperator } from "../../../../orchestrator/src/mcp/operator.ts";
import { loadMcpRuntimeConfiguration } from "../../../../orchestrator/src/mcp/runtime-config.ts";
import { hash, newRoot, put, readOwned, type Pin } from "./owned.ts";
import { address } from "./network.ts";
import type { Inputs } from "./preflight.ts";
import type { Identity } from "./identity.ts";

export async function generateConfiguration(
  inputs: Inputs,
  root: string,
  identity: Identity,
) {
  const configRoot = join(root, "configuration");
  await newRoot(configRoot);
  const save = (name: string, value: unknown) =>
    put(join(configRoot, name), JSON.stringify(value));
  const ca = (await readOwned(identity.ca.path)).toString();
  const authorities = ["legacy", "modern"]
    .flatMap((protocol) =>
      ["resource", "issuer", "authorization", "token"].map(
        (role) => `${role}.${protocol}.fixture.example:8443`,
      ),
    )
    .sort();
  const zone = {
    id: "fixture-zone",
    revision: "fixture-1",
    mode: "internal" as const,
    authorities,
    aliases: [],
    cidrs: [
      address(inputs, "modern") + "/32",
      address(inputs, "legacy") + "/32",
    ],
    denyCidrs: [
      inputs.nativeHostIp + "/32",
      address(inputs, "worker") + "/32",
      address(inputs, "coordinator") + "/32",
      address(inputs, "orchestrator") + "/32",
      "169.254.0.0/16",
    ],
    ca,
    limits: { execute: 8, discover: 2, oauth: 2, schema: 2, toolMs: 10000 },
  };
  const policy = new ZonePolicy(zone);
  const egressRules = zone.cidrs.map((cidr) => ({ cidr, except: [] }));
  validateEgressRules(egressRules, zone.denyCidrs, zone.cidrs, zone.mode);
  const infrastructure = {
    deploymentIdentity: `sha256:${hash(JSON.stringify({ runId: identity.runId, backend: inputs.backend, sourceCommit: inputs.sourceCommit }))}`,
    networkIdentity: `sha256:${hash(JSON.stringify({ subnet: inputs.subnet, nativeHostIp: inputs.nativeHostIp }))}`,
    trustIdentity: `sha256:${identity.ca.sha256}`,
    caContentDigests: { fixture: `sha256:${identity.ca.sha256}` },
  };
  const candidate = {
    version: 1,
    zones: {
      [zone.id]: {
        revision: zone.revision,
        worker_digest: `sha256:${policy.digest}`,
        authorities,
        aliases: [],
        configuration: JSON.parse(canonicalJson(zone)),
      },
    },
    deployment_identity: infrastructure.deploymentIdentity,
    network_identity: infrastructure.networkIdentity,
    trust_identity: infrastructure.trustIdentity,
    ca_content_digests: infrastructure.caContentDigests,
  };
  const candidateIdentity = `sha256:${hash(JSON.stringify(candidate))}`;
  // Reuse the production Rust-serialization/ZonePolicy identity verifier.
  parsePublishedOperator(
    Buffer.from(
      JSON.stringify({
        published: {
          generation: 1,
          expected_generation: null,
          identity: candidateIdentity,
          candidate,
        },
        pending: null,
        admission_open: true,
      }),
    ),
    infrastructure,
  );
  const candidateFile = await save("operator-candidate.json", candidate);
  const tls = (url: string) => ({
    url,
    caName: "fixture",
    certFile: identity.leaves.orchestrator.certificate.path,
    keyFile: identity.leaves.orchestrator.key.path,
    serverFingerprint: url.includes("worker.")
      ? identity.leaves.worker.fingerprint
      : identity.leaves.coordinator.fingerprint,
  });
  const registrations = ["modern", "legacy"].map((protocol) => ({
    id: `fixture-${protocol}`,
    clientId: "fixture-public",
    confidential: false,
    redirectUri: "https://app.fixture.example/integrations/mcp/callback",
    issuer: `https://issuer.${protocol}.fixture.example:8443`,
    resource: {
      zoneId: zone.id,
      endpoint: `https://resource.${protocol}.fixture.example:8443/mcp`,
    },
    scopes: ["tools:read", "tools:write"],
  }));
  const runtime = {
    version: 1,
    infrastructure: {
      deploymentIdentity: infrastructure.deploymentIdentity,
      networkIdentity: infrastructure.networkIdentity,
      trustIdentity: infrastructure.trustIdentity,
    },
    caFiles: { fixture: identity.ca.path },
    control: tls("https://control.fixture.example:8444"),
    workers: [
      {
        zoneId: zone.id,
        deploymentId: identity.runId,
        enabled: true,
        tls: tls("https://worker.fixture.example:8443"),
      },
    ],
    registrations,
  };
  const runtimeNormal = await save("orchestrator-normal.json", runtime);
  await loadMcpRuntimeConfiguration(runtimeNormal.path);
  const runtimeZoneDisabled = await save("orchestrator-zone-disabled.json", {
    ...runtime,
    workers: runtime.workers.map((worker) => ({ ...worker, enabled: false })),
  });
  await loadMcpRuntimeConfiguration(runtimeZoneDisabled.path);
  const worker = await save("worker.json", {
    zone,
    callers: [
      {
        role: "orchestrator",
        fingerprint: identity.leaves.orchestrator.fingerprint,
      },
      {
        role: "coordinator",
        fingerprint: identity.leaves.coordinator.fingerprint,
      },
    ],
    egressRules,
  });
  const clientIdentity = await put(
    join(configRoot, "coordinator-client.pem"),
    Buffer.concat([
      await readOwned(identity.leaves.coordinator.certificate.path),
      await readOwned(identity.leaves.coordinator.key.path),
    ]),
  );
  const destination = (endpoint: string) => ({ zoneId: zone.id, endpoint });
  const coordinator = await save("coordinator.json", {
    registry: {
      enabled: true,
      deploymentId: identity.runId,
      zones: {
        [zone.id]: {
          policyDigest: policy.digest,
          policyRevision: zone.revision,
          endpoints: ["legacy", "modern"]
            .flatMap((protocol) => [
              `https://resource.${protocol}.fixture.example:8443/mcp`,
              `https://resource.${protocol}.fixture.example:8443/.well-known/oauth-protected-resource`,
              `https://issuer.${protocol}.fixture.example:8443/.well-known/oauth-authorization-server`,
              `https://authorization.${protocol}.fixture.example:8443/authorize`,
              `https://token.${protocol}.fixture.example:8443/token`,
            ])
            .sort(),
        },
      },
      registrations: Object.fromEntries(
        registrations.map((registration) => {
          const protocol = registration.id.slice("fixture-".length);
          return [
            registration.id,
            {
              ...registration,
              resourceMetadata: destination(
                `https://resource.${protocol}.fixture.example:8443/.well-known/oauth-protected-resource`,
              ),
              authorizationMetadata: destination(
                `https://issuer.${protocol}.fixture.example:8443/.well-known/oauth-authorization-server`,
              ),
              authorization: destination(
                `https://authorization.${protocol}.fixture.example:8443/authorize`,
              ),
              token: destination(
                `https://token.${protocol}.fixture.example:8443/token`,
              ),
              issuerParameterRequired: true,
              accountField: "account",
            },
          ];
        }),
      ),
    },
    listenAddr: "0.0.0.0:8444",
    serverCertificateFile: identity.leaves.coordinator.certificate.path,
    serverPrivateKeyFile: identity.leaves.coordinator.key.path,
    orchestratorCaFile: identity.ca.path,
    orchestratorFingerprints: [identity.leaves.orchestrator.fingerprint],
    workers: {
      [zone.id]: {
        url: "https://worker.fixture.example:8443",
        serverCaFile: identity.ca.path,
        clientIdentityFile: clientIdentity.path,
      },
    },
    publication: {
      listenAddr: "0.0.0.0:8445",
      serverCertificateFile: identity.leaves.coordinator.certificate.path,
      serverPrivateKeyFile: identity.leaves.coordinator.key.path,
      operatorCaFile: identity.ca.path,
      operatorFingerprints: [identity.leaves.operator.fingerprint],
      candidateFile: candidateFile.path,
      caFiles: { fixture: identity.ca.path },
    },
  });
  const hostTls = await save("host-tls.json", {
    certificate: identity.leaves.host.certificate.path,
    key: identity.leaves.host.key.path,
    client_ca: identity.ca.path,
    coordinator_pins: [identity.leaves.coordinator.hostPin],
  });
  const coordinatorHostTls = await save("coordinator-host-tls.json", {
    client_certificate: identity.leaves.coordinator.certificate.path,
    client_key: identity.leaves.coordinator.key.path,
    hosts: {
      [identity.hostId]: {
        endpoint: "https://host.fixture.example:9101",
        server_name: "host.fixture.example",
        server_ca: identity.ca.path,
        server_pins: [identity.leaves.host.hostPin],
      },
    },
  });
  return {
    zone,
    infrastructure,
    candidateFile,
    candidateIdentity,
    runtimeNormal,
    runtimeZoneDisabled,
    worker,
    coordinator,
    clientIdentity,
    hostTls,
    coordinatorHostTls,
  };
}
