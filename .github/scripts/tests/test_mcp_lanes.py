import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / 'detect-rebake-lanes.py'
spec = importlib.util.spec_from_file_location('lanes', SCRIPT)
lanes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lanes)


def detect(path, crates=None, closure=None):
    with tempfile.NamedTemporaryFile() as output, patch.object(lanes, 'cargo_meta', return_value={}), \
         patch.object(lanes, 'crate_dirs', return_value={}), \
         patch.object(lanes, 'changed_crates', return_value=crates or set()), \
         patch.object(lanes, 'release_closure', return_value=closure or set()), \
         patch.object(sys, 'argv', ['detect', '--stdin']), \
         patch.object(sys, 'stdin', io.StringIO(path + '\n')), \
         patch.dict(os.environ, {'GITHUB_OUTPUT': output.name}), \
         contextlib.redirect_stderr(io.StringIO()):
        lanes.main()
        return dict(line.split('=', 1) for line in Path(output.name).read_text().splitlines())


class McpLanes(unittest.TestCase):
    def test_worker_runs_consumers_and_rebuilds_images(self):
        result = detect('mcp-worker/src/contracts.ts')
        self.assertEqual(result['test_rust'], 'true')
        self.assertEqual(result['test_orchestrator'], 'true')
        self.assertEqual(result['test_mcp_worker'], 'true')
        self.assertEqual(json.loads(result['images_matrix']), ['orchestrator', 'mcp-worker'])
        self.assertEqual(result['node_assets'], 'false')

    def test_fixture_selects_native_and_static_gates(self):
        result = detect('tests/fixtures/mcp-model-cli/src/main.rs')
        self.assertEqual(result['test_rust'], 'true')
        self.assertEqual(result['test_cross'], 'true')
        self.assertEqual(result['node_assets'], 'false')

    def test_harness_and_shared_protocol_select_fixture_consumers(self):
        for name in ['engram-harness-codex', 'engram-harness-claude', 'engram-harness-proto']:
            with self.subTest(name=name):
                result = detect(f'crates/{name}/src/main.rs', {name}, {name})
                self.assertEqual(result['test_rust'], 'true')
                self.assertEqual(result['test_cross'], 'true')

    def test_linux_wrapper_target_and_gate_select_linux_lane(self):
        result = detect('crates/engram-harness-claude/tests/linux_mcp_entrypoint.rs',
                        {'engram-harness-claude'}, {'engram-harness-claude'})
        self.assertEqual(result['test_rust'], 'true')
        self.assertEqual(result['test_cross'], 'true')
        self.assertEqual(detect('.github/scripts/mcp-acceptance-linux.sh')['test_rust'], 'true')

    def test_closed_namespace_source_and_gate_select_firecracker_lane(self):
        source = 'crates/engram-sandbox-firecracker/src/managed/network/linux.rs'
        result = detect(source, {'engram-sandbox-firecracker'}, {'engram-sandbox-firecracker'})
        self.assertEqual(result['test_fc'], 'true')
        self.assertEqual(detect('.github/scripts/mcp-acceptance-fc-closed.sh')['test_fc'], 'true')

    def test_closed_namespace_gate_is_required_and_unconditional(self):
        workflow = (SCRIPT.parents[1] / 'workflows/ci.yml').read_text()
        lane = workflow.split('  test-fc-nbd-net:', 1)[1].split('  test-fc-e2e:', 1)[0]
        step = lane.split('      - name: CLOSED namespace installed-state test (root)', 1)[1].split('      - name:', 1)[0]
        self.assertIn('run: bash .github/scripts/mcp-acceptance-fc-closed.sh', step)
        self.assertNotIn('        if:', step)
        self.assertIn('      - test-fc-nbd-net', workflow.split('  ci-gate:', 1)[1])

    def test_helper_runs_native_gate(self):
        result = detect('sandbox-netstack/go.sum')
        self.assertEqual(result['test_netstack'], 'true')
        self.assertEqual(result['test_rust'], 'true')
        self.assertEqual(result['test_fc'], 'true')
        self.assertEqual(result['e2e'], 'true')
        self.assertEqual(result['node_assets'], 'false')

    def test_helper_source_and_gate_select_artifact_and_vz_lanes(self):
        for path, crates in [
            ('crates/engram-sandbox-vz/src/managed/helper.rs', {'engram-sandbox-vz'}),
            ('.github/scripts/mcp-acceptance-helper.sh', set()),
            ('.github/workflows/ci.yml', set()),
        ]:
            with self.subTest(path=path):
                result = detect(path, crates, crates)
                self.assertEqual(result['test_rust'], 'true')
                self.assertEqual(result['test_netstack'], 'true')

    def test_stack_fixture_selects_worker_and_orchestrator_gates(self):
        for path in ['tests/fixtures/mcp-stack/main.mts',
                     'tests/fixtures/mcp-stack/durable.test.mts',
                     'tests/fixtures/mcp-stack/process.test.mts',
                     'tests/fixtures/mcp-stack/tsconfig.json',
                     'tests/fixtures/mcp-stack/platform/supervisor.ts',
                     'tests/fixtures/mcp-stack/platform/platform.test.ts',
                     'tests/fixtures/mcp-stack/platform/composition.test.ts',
                     'tests/fixtures/mcp-stack/platform/tsconfig.json']:
            with self.subTest(path=path):
                result = detect(path)
                self.assertEqual(result['test_mcp_worker'], 'true')
                self.assertEqual(result['test_orchestrator'], 'true')

    def test_source_gate_paths_select_required_jobs(self):
        for path in ['orchestrator/scripts/mcp-stack/run.ts',
                     'orchestrator/scripts/mcp-stack/tsconfig.json',
                     '.github/scripts/mcp-acceptance-native.sh',
                     '.github/scripts/tests/test_mcp_source_gates.py',
                     '.github/workflows/ci.yml']:
            with self.subTest(path=path):
                self.assertEqual(detect(path)['test_orchestrator'], 'true')
        workflow = (SCRIPT.parents[1] / 'workflows/ci.yml').read_text()
        worker = workflow.split('  mcp-worker:', 1)[1].split('  netstack:', 1)[0]
        self.assertIn("if: needs.detect.outputs.test_mcp_worker == 'true'", worker)
        self.assertIn('node-version: 24.22.2', worker)
        self.assertIn('mkdir -p var/mcp-stack-fixture', worker)
        self.assertIn('node --test --test-concurrency=1 --test-timeout=30000', worker)
        self.assertIn('tests/fixtures/mcp-stack/durable.test.mts', worker)
        self.assertIn('tests/fixtures/mcp-stack/process.test.mts', worker)
        self.assertNotIn('mcp-worker/test/fixture.test.ts', worker)
        orch = workflow.split('  orchestrator:', 1)[1].split('  buf:', 1)[0]
        self.assertIn("if: needs.detect.outputs.test_orchestrator == 'true'", orch)
        self.assertIn('run: bun run typecheck', orch)
        self.assertIn('run: bun run tsc -p scripts/mcp-stack/tsconfig.json --noEmit', orch)
        self.assertIn('bun test scripts/mcp-stack', orch)
        self.assertIn('0 fail', orch)
        self.assertIn('run: bun run tsc -p ../tests/fixtures/mcp-stack/platform/tsconfig.json --noEmit', orch)
        platform = orch.split('      - name: MCP platform source unit tests', 1)[1].split('      - name:', 1)[0]
        self.assertIn('bun test ../tests/fixtures/mcp-stack/platform', platform)
        self.assertNotIn('        if:', platform)
        project = json.loads((SCRIPT.parents[2] / 'tests/fixtures/mcp-stack/platform/tsconfig.json').read_text())
        self.assertEqual(project['extends'], '../../../../orchestrator/tsconfig.json')
        self.assertEqual(project['include'], ['./*.ts'])
        settings = json.loads((SCRIPT.parents[2] / 'orchestrator/tsconfig.json').read_text())
        self.assertTrue(settings['compilerOptions']['strict'])
        gate = workflow.split('  ci-gate:', 1)[1]
        for lane in ['orchestrator', 'mcp-worker', 'test-linux', 'test-macos']:
            self.assertIn(f'      - {lane}\n', gate)

    def test_complete_physical_producer_selector(self):
        for path in ['tests/fixtures/mcp-stack/platform/ci/linux-assets.Dockerfile',
                     '.github/scripts/mcp-stack-assets.py', '.github/scripts/mcp-stack-inputs.mts',
                     '.github/scripts/mcp-stack-fc.sh', 'docker/coordinator.Dockerfile',
                     'deploy/bundles/agentd/build.sh', 'deploy/demo/Dockerfile',
                     'deploy/kernel/build-fc-kernel.sh', 'third_party/firecracker',
                     'sandbox-netstack/build.sh', '.sqlx/query.json', 'deploy/migrations/100.sql',
                     'orchestrator/scripts/mcp-stack/run.ts', 'mcp-worker/package-lock.json',
                     'crates/engram-host-agent/src/lib.rs', '.bun-version', '.github/workflows/ci.yml']:
            with self.subTest(path=path):
                self.assertEqual(detect(path)['test_mcp_physical'], 'true')
        for path in ['README.md', 'web/src/App.tsx', 'site/src/content/docs/index.mdx']:
            with self.subTest(path=path):
                self.assertEqual(detect(path)['test_mcp_physical'], 'false')

    def test_unrelated_path_does_not_run_new_lanes(self):
        result = detect('README.md')
        self.assertEqual(result['test_mcp_worker'], 'false')
        self.assertEqual(result['test_netstack'], 'false')

    def test_callback_canary_runs_for_its_profiles_runtime_and_script(self):
        for path in ['deploy/helm/engram/templates/web-configmap.yaml',
                     'deploy/helm/engram/values-aws.yaml.example',
                     'deploy/terraform/gcp/quickstart/eso.tf',
                     'deploy/helm/engram/ci/callback-ingress-canary.py',
                     'docker/web.Dockerfile']:
            with self.subTest(path=path):
                self.assertEqual(detect(path)['test_deploy'], 'true')
        self.assertEqual(detect('README.md')['test_deploy'], 'false')

if __name__ == '__main__':
    unittest.main()
