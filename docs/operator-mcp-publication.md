# Publish an external MCP operator configuration

The coordinator can run a separate operator mTLS listener. Its client CA and
exact SHA-256 leaf pins identify the operator. Operator pins must not overlap
with orchestrator pins. Shared operator-controlled PKI roots are allowed; the
verified leaf pin and listener role determine authorization. The application and orchestrator listeners do not serve
the operator mutation service. The service accepts no caller identity header.

The optional `publication` object in the coordinator MCP configuration contains:

```json
{
  "listenAddr": "127.0.0.1:50063",
  "serverCertificateFile": "/run/mcp-operator/server.crt",
  "serverPrivateKeyFile": "/run/mcp-operator/server.key",
  "operatorCaFile": "/run/mcp-operator/client-ca.crt",
  "operatorFingerprints": ["REPLACE_WITH_UPPERCASE_COLON_SEPARATED_SHA256_LEAF_PIN"],
  "candidateFile": "/run/mcp-operator/candidate.json",
  "caFiles": { "worker-root": "/run/mcp-worker/server-ca.crt" }
}
```

This is a file fragment, not an enabled deployment. Omitting `publication` starts
no operator listener or scanner. Configuring it does not enable the MCP registry.
The listener can reconcile a staged generation while the registry is disabled.
All input files have a 128 KiB limit. Keep private keys in the dedicated identity
files. Candidate contents and published projections must contain only policy.

The candidate uses the `OperatorCandidate` JSON shape in `engram-core`:

- `version` is 1. `zones` maps every selectable zone ID to its exact revision,
  `worker_digest`, sorted unique `authorities`, sorted unique `aliases`, and
  `configuration`.
- `configuration` is the worker zone policy: `id`, `revision`, `mode`,
  `authorities`, `aliases`, `cidrs`, `denyCidrs`, optional `ca`, and optional
  `limits`. Unknown fields are rejected. Authorities are canonical DNS hosts,
  with an optional nondefault port. Aliases are canonical DNS hosts.
- `deployment_identity`, `network_identity`, and `trust_identity` are distinct
  identity fields. A worker digest is not evidence of network enforcement.
- `ca_content_digests` maps named CA inputs to SHA-256 digests of their actual
  bytes. `caFiles` must have the same names and contents. A path hash cannot
  replace a content hash. Inline zone CA contents must match one of these hashes.

Identity digests use `sha256:` followed by 64 lowercase hexadecimal digits. The
candidate identity is the SHA-256 digest of the exact compact serialization of
`OperatorCandidate`: struct fields use their declared order, maps use sorted
keys, and nested configuration objects use sorted keys. Preserve all array order
in `configuration`. Use `OperatorCandidate::identity()` to compute the staging
identity. It includes policy changes that do not change the hostname set, such
as CIDRs, limits, CA contents, and deployment identities. The worker digest is a
separate value that its authenticated handshake must validate.

`engram.mcp.v1.OperatorPublicationService.Stage` accepts protocol 1, an optional
`expected_generation`, and `configured_candidate_identity`. It resolves the
candidate from the operator-owned file. It does not accept policy bytes. Use an
absent expected generation for the first publication. An exact retry returns
the existing pending or completed generation. A different pending candidate or
stale generation fails. `Read` returns the durable published and pending state.
There is no Publish RPC.

Staging closes managed creation and boot/start admission in the same transaction
that saves the candidate. The scanner enumerates all managed creations, boot
attempts, and retained managed policies, including idle, queued, and terminal
rows. It stores the union of historical denies and every candidate authority and
alias before host I/O. A policy-only generation change still advances the deny
revision once. A restart resumes from this durable requirement.

Readiness requires exact authenticated host evidence for the current source and
placement. A permanent Retired receipt proves that the old owner is nonrunning;
it is not a new applied policy ACK or a capture. A Captured receipt proves the
exact source was destroyed, but publication does not create a recovery artifact.
A held creation with no allocation or boot attempt can use its durable next-boot
requirement. Missing owners, errors, timeouts, and terminal session labels do not
prove readiness. Policy or placement changes invalidate a saved observation.
The final transaction locks and checks all participants again before it publishes
the generation and opens admission. No host call holds a database transaction.

The pinned orchestrator `SessionPolicyService.ReadPublishedOperator` returns a method-specific
`ReadPublishedOperatorResponse` with the same protocol-1 JSON payload only when admission is open and no candidate is
pending. It returns the published record with its complete configuration. Its
consumer must compare the worker digest to a validated worker handshake and
validate the separate deployment, network, and trust projection before selection.
Configuration hashes do not certify an enforcing CNI or physical guest isolation.
Those properties require deployment and backend acceptance tests.
