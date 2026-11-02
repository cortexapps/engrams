# MCP model CLI fixture

This test-only program replaces the model CLI. It does not replace either
harness or authorize an MCP call. Claude uses the generated strict MCP config
and hook settings. Codex uses app-server JSON-RPC. Both use the call ID
`fixture-call` and send `{"fixture":"<mode>"}` as tool arguments. The selected
manifest tool must accept that input and have no native binding. Use a neutral
fixture tool name, description, and data.

Set these variables for each fixture process:

- `ENGRAM_FIXTURE_TOOL`: the exact manifest tool name.
- `ENGRAM_FIXTURE_MODE`: `read`, `write`, `denied`, `refire`, `held-call`, or
  `forged` (call an unlisted name and require wrapper rejection).
  The mode is an input sentinel. Policy and the harness determine the outcome.
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

The proof contains only a fixed call ID, the fixture tool name, booleans for
the neutral `sentinel`, `second sentinel`, and `{"fixture":true}` result
values, and bounded native outcome and delivery fields. It records a CLI-side
observation. It is not evidence that the journal or an upstream service
committed a result. Empty proof means no result was recorded. Never reuse a
proof path. The fixture does not print the input environment, arguments, or
tool payloads. `--fixture-version` prints `engram-mcp-model-fixture 1`.

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
