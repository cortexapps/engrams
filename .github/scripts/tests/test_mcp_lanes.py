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


def detect(path):
    with tempfile.NamedTemporaryFile() as output, patch.object(lanes, 'cargo_meta', return_value={}), \
         patch.object(lanes, 'crate_dirs', return_value={}), \
         patch.object(lanes, 'changed_crates', return_value=set()), \
         patch.object(lanes, 'release_closure', return_value=set()), \
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

    def test_helper_runs_native_gate(self):
        result = detect('sandbox-netstack/go.sum')
        self.assertEqual(result['test_netstack'], 'true')
        self.assertEqual(result['test_rust'], 'true')
        self.assertEqual(result['test_fc'], 'true')
        self.assertEqual(result['e2e'], 'true')
        self.assertEqual(result['node_assets'], 'false')

    def test_unrelated_path_does_not_run_new_lanes(self):
        result = detect('README.md')
        self.assertEqual(result['test_mcp_worker'], 'false')
        self.assertEqual(result['test_netstack'], 'false')

if __name__ == '__main__':
    unittest.main()
