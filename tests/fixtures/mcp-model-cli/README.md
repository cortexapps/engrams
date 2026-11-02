# MCP model CLI fixture

This test-only program replaces the model CLI. It does not replace either
harness or authorize an MCP call. Claude uses the generated strict MCP config
and hook settings. Codex uses app-server JSON-RPC. By default, both use the call ID
`fixture-call` and send `{"fixture":"<mode>"}` as tool arguments. The selected
manifest tool must accept that input and have no native binding. Use a neutral
fixture tool name, description, and data.

Set these variables for each fixture process:

- `ENGRAM_FIXTURE_TOOL`: the exact manifest tool name.
- `ENGRAM_FIXTURE_MODE`: `read`, `write`, `denied`, `refire`, `held-call`, or
  `forged` (call an unlisted name and require wrapper rejection).
  The mode is an input sentinel. Policy and the harness determine the outcome.
- `ENGRAM_FIXTURE_ARGUMENTS_JSON`: optional JSON object, at most 4096 UTF-8
  bytes and 16 nested containers (the root object has depth 1). Duplicate keys
  at any depth, trailing data, and non-object values are rejected. The exact
  parsed value is used in the assistant frame, hook, bridge, and dynamic call.
- `ENGRAM_FIXTURE_CALL_ID`: optional ASCII `[A-Za-z0-9_-]{1,64}`. All call and
  proof locations use it, including a resumed or repeated call. The default
  remains `fixture-call`.
- `ENGRAM_FIXTURE_EXPECTED_RESULT_JSON`: optional literal native result object,
  at most 1 MiB. Duplicate keys and excessive parser depth are rejected.
  Compare the actual native object, as described below. Do not put private
  data in the environment. The operating system can impose a smaller total
  environment limit than this parser limit.
- `ENGRAM_FIXTURE_STATE_DIR`: an absolute, private mode-0700 directory owned by
  the test. No component may be a symlink.
- `ENGRAM_FIXTURE_PROOF`: a new direct child of that directory. The fixture
  creates it exclusively. An optional single `{pid}` in the basename expands
  to the decimal fixture process ID. The parent is checked before expansion.
  This creates distinct proofs when a wrapper restarts its CLI; it is not a
  production process identity. Without the token, existing behavior is unchanged.
- `ENGRAM_FIXTURE_BARRIER`: optional direct child of that directory. While it is
  absent, a call waits for up to 30 seconds; an interrupt can end the wait.
  Create a regular file at that path to release it. A symlink is rejected.

The default proof contains only the call ID, the fixture tool name, booleans for
the neutral `sentinel`, `second sentinel`, and `{"fixture":true}` result
values, and bounded native outcome and delivery fields. It records a CLI-side
observation. It is not evidence that the journal or an upstream service
committed a result. Empty proof means no result was recorded. Never reuse a
proof path. The fixture does not print the input environment, arguments, or
tool payloads. `--fixture-version` prints `engram-mcp-model-fixture 1`.

With any of the three optional scalar variables set, proof also records
`selectedToolName`, `selectedSchemaSha256`, and `exposedMcpToolNamesSha256`.
The first two come from the single matching
member of the actual Claude `tools/list` or Codex `dynamicTools` array. The caller
must verify the alias and schema against the public session binding before the
prompt. The exposure hash covers the sorted array of actual exposed names that
start with `mcp_`. It rejects invalid names, duplicates, and more than 128 MCP
names. The caller compares this hash against the complete public binding set.
This fixture does not select tools by description or infer their alias.

When an expected object is configured and native output is available, proof adds
`resultMatchesExpected`, `resultSha256`, and `expectedResultSha256`. A mismatch
records `false`. SHA-256 values have the prefix `sha256:` and 64 lowercase hex
digits. Hash input is compact JSON with recursively sorted object keys. Array
order, absent fields, and text strings are preserved. No arguments, expected
values, or received output bytes are stored in proof or error output.

Claude compares the JSON-RPC `result` object, including `_meta`. Codex compares
the complete `item/tool/call` response `result` object, including `success` and
all `contentItems`. JSON text inside those items remains literal text. For
example, a count of 1 with a successful receipt and `resultUnavailable=false`
has these two expected objects:

```json
{"content":[{"type":"text","text":"count=1"}],"structuredContent":{"count":1},"isError":false,"_meta":{"engrams/toolResult":{"outcome":"succeeded","delivery":"available","resultUnavailable":false,"isError":false}}}
```

```json
{"success":true,"contentItems":[{"type":"inputText","text":"count=1"},{"type":"inputText","text":"{\"structuredContent\":{\"count\":1}}"},{"type":"inputText","text":"{\"delivery\":\"available\",\"isError\":false,\"outcome\":\"succeeded\",\"resultUnavailable\":false}"}]}
```

Only fields actually sent by the runtime belong in the expected value. In
particular, do not add an omitted `resultUnavailable` or `isError` field.
An unavailable delivery has no comparison fields. A transport error does not
become a tool error. The existing receipt fields remain separate, and a hash
match does not prove an upstream effect. The legacy `resultReceived` flag says
that a native completion was observed, including an unavailable receipt; it
does not establish that tool output is available. Codex receipt flags use the
last text item, which the native encoder reserves for receipt metadata.

A scalar configuration has one fixed call ID and one fixed tool. For a bounded
sequence, set `ENGRAM_FIXTURE_CALL_PLAN_JSON` instead of TOOL, ARGUMENTS_JSON,
CALL_ID and EXPECTED_RESULT_JSON. These scalar variables must be absent in plan
mode. The plan is at most 65536 UTF-8 bytes and has exactly a `steps` array of
1 to 16 objects. Each step has `marker`, `tool`, `arguments`, `callId`,
`resultFormat` (`mcp` or `json`), and optional `expectedResult`. Unknown fields
are rejected. Markers and call IDs use the call-ID grammar and must be unique.
Arguments have the scalar size and depth bounds. The whole-plan size limit also
bounds the optional literal expected native objects.

The current user prompt must contain exactly one `[[fixture:MARKER]]` token.
Claude reads user `message.content` text; Codex reads current `turn/start` input
text items. A missing, unknown, repeated, or ambiguous token fails closed. Tool
result blocks cannot select a step. The same marker always selects the same
fixed tool, arguments, and call ID. A new marker can select a new call ID in the
same session. A fixed `spawn_session` message can contain a child marker, so the
child selects its MCP read step rather than the spawn step. No argument or
command interpolation is supported.

```json
{"steps":[{"marker":"read","tool":"mcp_example_01234567890123456789","arguments":{},"callId":"read-1","resultFormat":"mcp"},{"marker":"spawn","tool":"spawn_session","arguments":{"task_name":"fixture-child","message":"[[fixture:read]]","idempotency_key":"child-1"},"callId":"spawn-1","resultFormat":"json"}]}
```

Plan proof includes `planSha256`, the canonical digest of the entire immutable
plan. A native JSON step hashes its literal native output even without an
expected object. With an expected object, it also records the comparison and
expected hash. Claude native JSON output is `{"content":[{"type":"text",
"text":"<literal JSON text>"}]}`. Codex uses `{"success":true,"contentItems":
[{"type":"inputText","text":"<literal JSON text>"}]}`. Native JSON proof has
no MCP receipt fields; Codex retains its actual `success` flag. The fixture does
not decode those text strings or assert that a child was actually created.

Each completion appends one JSON line to the same process proof file and
flushes it before the native turn-complete frame. With `{pid}` in the filename,
a restarted process gets a new file. Read proof as JSONL when there are multiple
turns; use the call ID and plan digest to select the required observation.

A direct child of the private state directory, `plan-selection.json`, contains
only planSha256, harness, mode, marker and pending. The fixture writes this
receipt atomically before it emits a call. It marks the selection completed
only after an actual native response. It stores no arguments or result bytes.
A deferred Claude `--resume` in write/refire mode has no current prompt; it can
repeat only a pending selection for the exact same plan, harness and mode.
Missing, completed, malformed, mismatched or symlink receipts fail closed.
A replay cannot advance to another step. A new ID requires a current marked
prompt. This is a test-client retry; the production journal remains responsible
for authorization, idempotency and effects. A session needs its own state
directory, including when a parent and child use the same fixed plan.

Build this standalone workspace and run its nonignored protocol tests:

```sh
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
cargo test --locked --manifest-path tests/fixtures/mcp-model-cli/Cargo.toml --test protocol
cargo build --locked --release --target aarch64-unknown-linux-musl \
  --manifest-path tests/fixtures/mcp-model-cli/Cargo.toml --bin engram-mcp-model-fixture
file "$CARGO_TARGET_DIR/aarch64-unknown-linux-musl/release/engram-mcp-model-fixture"
shasum -a 256 "$CARGO_TARGET_DIR/aarch64-unknown-linux-musl/release/engram-mcp-model-fixture"
```

Run these commands from the repository root and set `CARGO_TARGET_DIR` to a
different owned directory if the root target is shared.
Stage the exact build and verify its digest. This workspace is excluded from
the main workspace, so normal workspace nextest does not run its tests. CI must
trigger and invoke this standalone target for changes in this directory and for
changes to either harness's CLI protocol. It must fail if the test count is
zero or the expected artifact is missing.

The existing Codex adapter has an explicit fixture gate. Build the fixture for
the host first, then invoke the gate once with `ENGRAM_FIXTURE_MODE=read` and
again with `ENGRAM_FIXTURE_MODE=held-call`. For each run, set all required
variables above, `ENGRAM_FIXTURE_BIN` to the absolute path of that host binary,
and `ENGRAM_FIXTURE_TOOL=remote_inventory`. Use a fresh proof path and state
directory each time. Run:

```sh
cargo test --locked -p engram-harness-codex --bin engram-harness-codex \
  tests::external_mcp_static_fixture_adapter_gate -- --ignored --exact
```

Require exactly one selected and passed test in each run. The gate checks the
fixture version, real adapter call and result, or cancellation event and
interrupted turn. It does not boot a guest or call a provider. Claude's local
fixture tests use a mock subprocess; they do not prove the production bridge.
The Linux guest gate must stage the current production Claude wrapper and this
fixture, set `ENGRAM_CLAUDE_BIN` to the staged fixture, supply the immutable
manifest and the state variables above, and drive the real harness transport.
It must verify the actual hook, bridge, result and cancellation paths, then
repeat the required cold and full-stack cases. A local fixture pass cannot
replace that gate. Remove only the test-owned state and staged files after all
children have exited.

The additive fixture contract still uses version 1. Claude emits its neutral
native assistant tool-use frame before the hook. In `write` and `refire` modes,
`--resume` makes it reissue the same call once without a new stdin prompt,
as the production deferred-result path expects. Other modes still wait for a
prompt. A harness test bounds launch count to four and uses private state.

On Linux, `.github/scripts/mcp-acceptance-linux.sh` builds both current shipping
wrapper binaries and the fixture, checks executable ELF artifacts and fixture
version, then runs exactly eight integration tests in `linux_mcp_entrypoint`.
Each test exercises both wrappers through TCP `HarnessAttach`, sequenced events,
and event ACKs. It uses a fresh environment and owned state. The refire case
kills and observes the reaped direct model child; the wrapper remains alive.
Claude resumes the deferred call; Codex resumes its thread and a new turn
reissues the same ID. The latter emits a second host request; server journal
idempotency is outside this test. The result sender is the test host, so denial
receipt delivery is not evidence of server authorization. Forged names are
rejected by the actual wrappers before a host request. Cancellation checks the
signal and absent model result, without a late result or an external effect.

This process test closes the shipping Linux entrypoint gap. Real guest,
Firecracker/VZ isolation, cold restore, authenticated server policy, worker
network enforcement, journal/outbox durability, and pinned vendor CLI behavior
remain separate acceptance gates. `HarnessAttach` does not negotiate the global
wire version. CI builds the test and wrappers from the same source revision.

The Linux gate runs under the test-only `linux_mcp_supervisor` Cargo example.
Before starting the test driver, it becomes a Linux child subreaper. A forced
100 ms deadline check starts both real wrappers with held calls, kills the test
process with SIGKILL, and confirms that its four observed wrapper/model children
remain alive in separate groups. The supervisor then kills only its own
kernel-reported direct children, reaps adopted descendants until `ECHILD`, and
checks that the observed processes are gone before it removes state. It repeats
this cleanup after the ordinary eight-case matrix, including timeout/failure.

The matrix has a 100 second internal deadline; cleanup waits are bounded.
A 240 second hard outer deadline bounds supervisor failure. The shell removes
state only after the supervisor writes its cleanup-complete marker. If cleanup
cannot be confirmed, the gate fails and retains state. The supervisor is an
explicitly built example, not a production binary or a libtest discovery target.
The deliberately held `supervisor_timeout_fixture` test is ignored and excluded
from the eight-case matrix; only the supervisor runs it.
