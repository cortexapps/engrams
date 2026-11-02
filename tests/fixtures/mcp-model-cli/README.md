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
  `forged` (Claude only: call an unlisted name and require rejection).
  The mode is an input sentinel. Policy and the harness determine the outcome.
- `ENGRAM_FIXTURE_STATE_DIR`: an absolute, private mode-0700 directory owned by
  the test. No component may be a symlink.
- `ENGRAM_FIXTURE_PROOF`: a new direct child of that directory. The fixture
  creates it exclusively. Use a new path for each launch or refire.
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
cargo test --locked --manifest-path tests/fixtures/mcp-model-cli/Cargo.toml --test protocol
cargo build --locked --release --target aarch64-unknown-linux-musl \
  --manifest-path tests/fixtures/mcp-model-cli/Cargo.toml --bin engram-mcp-model-fixture
file target/aarch64-unknown-linux-musl/release/engram-mcp-model-fixture
shasum -a 256 target/aarch64-unknown-linux-musl/release/engram-mcp-model-fixture
```

Set `CARGO_TARGET_DIR` to an owned directory if the default target is shared.
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
