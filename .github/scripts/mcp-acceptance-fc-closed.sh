#!/usr/bin/env bash
# Run the installed CLOSED namespace check on the isolated Linux CI worker.
set -euo pipefail
[[ $(uname -s) == Linux ]] || { echo 'CLOSED namespace gate requires Linux' >&2; exit 1; }
for tool in cargo python3 sudo ip sysctl nft; do
  command -v "$tool" >/dev/null || { echo "Missing required tool: $tool" >&2; exit 1; }
done
name=managed::network::linux::tests::actual_closed_namespace_readback_detects_route_tampering
state=$(mktemp -d)
trap 'rm -rf "$state"' EXIT
# Cargo builds as the runner; only its current libtest executable runs as root.
export CARGO_TARGET_DIR="$PWD/target"
cargo test --locked -p engram-sandbox-firecracker --lib --no-run --message-format=json > "$state/artifacts.jsonl"
artifact=$(ARTIFACT_LIST="$state/artifacts.jsonl" python3 - "$PWD" "${CARGO_TARGET_DIR:-$PWD/target}" < /dev/null <<'PY'
import json, os, pathlib, stat, sys
workspace, target = map(pathlib.Path, sys.argv[1:])
workspace = workspace.resolve()
target = (workspace / target).resolve()
assert target.is_relative_to(workspace), 'target must be inside this checkout'
with open(os.environ['ARTIFACT_LIST']) as stream:
    paths = [m['executable'] for line in stream if (m := json.loads(line)).get('reason') == 'compiler-artifact'
             and m.get('executable') and m['target']['name'] == 'engram_sandbox_firecracker'
             and pathlib.Path(m['target']['src_path']).resolve() == workspace / 'crates/engram-sandbox-firecracker/src/lib.rs'
             and m['profile']['test']]
assert len(paths) == 1, 'one current Firecracker libtest artifact is required'
p = pathlib.Path(paths[0]).resolve(strict=True)
s = p.stat()
assert p.is_relative_to(target) and stat.S_ISREG(s.st_mode) and s.st_uid == os.getuid()
assert not s.st_mode & 0o022 and os.access(p, os.X_OK), 'unsafe test artifact'
print(p)
PY
)
# Check selection before sudo, then require a positive result from the same artifact.
"$artifact" --ignored --exact "$name" --list > "$state/selected"
grep -Fxq "$name: test" "$state/selected"
grep -Fxq '1 test, 0 benchmarks' "$state/selected"
[[ $(grep -c ': test$' "$state/selected") == 1 ]] || { echo 'Expected exactly one CLOSED namespace test' >&2; exit 1; }
sudo env PATH=/usr/sbin:/usr/bin:/sbin:/bin "$artifact" --ignored --exact "$name" --nocapture 2>&1 | tee "$state/result"
grep -Fxq 'running 1 test' "$state/result"
grep -Fxq "test $name ... ok" "$state/result"
grep -Eq '^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; [0-9]+ filtered out;' "$state/result"
