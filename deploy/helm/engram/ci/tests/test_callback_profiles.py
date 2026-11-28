"""Check the rendered public ingress profiles, without a cloud account."""

import json
from pathlib import Path
import re
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[5]
CHART = ROOT / 'deploy/helm/engram'


def yaml_documents(source):
    result = subprocess.run(
        ['ruby', '-ryaml', '-rjson', '-e',
         'puts JSON.generate(YAML.load_stream(STDIN.read))'],
        input=source, text=True, capture_output=True, check=True, timeout=10,
    )
    return json.loads(result.stdout)


def render(overlay):
    source = re.sub(r'REPLACE_[A-Z_]+', 'stub', (CHART / overlay).read_text())
    result = subprocess.run(
        ['helm', 'template', 'callback-canary', str(CHART), '-f',
         str(CHART / 'ci/render-values.yaml'), '-f', '-'],
        input=source, text=True, capture_output=True, check=True, timeout=30,
    )
    return yaml_documents(result.stdout)


class CallbackProfiles(unittest.TestCase):
    def test_aws_disables_access_logging_in_rendered_ingress(self):
        ingresses = [d for d in render('values-aws.yaml.example')
                     if d and d['kind'] == 'Ingress']
        self.assertGreater(len(ingresses), 0)
        for ingress in ingresses:
            attributes = dict(item.split('=', 1) for item in ingress['metadata']
                              ['annotations']['alb.ingress.kubernetes.io/load-balancer-attributes'].split(','))
            self.assertEqual(attributes.get('access_logs.s3.enabled'), 'false')
            self.assertEqual(attributes['idle_timeout.timeout_seconds'], '3600')

    def test_gcp_quickstart_disables_access_logging_on_referenced_backend(self):
        source = (ROOT / 'deploy/terraform/gcp/quickstart/eso.tf').read_text()
        block = source.split('resource "kubectl_manifest" "web_backendconfig" {', 1)[1]
        manifest = block.split('<<-YAML\n', 1)[1].split('\n  YAML', 1)[0]
        backend, = yaml_documents(manifest)
        services = [d for d in render('values-gcp.yaml.example')
                    if d and d['kind'] == 'Service' and
                    'cloud.google.com/backend-config' in d['metadata'].get('annotations', {})]
        self.assertGreater(len(services), 0)
        for service in services:
            reference = json.loads(service['metadata']['annotations']['cloud.google.com/backend-config'])
            self.assertEqual(reference['default'], backend['metadata']['name'])
        self.assertIs(backend['spec'].get('logging', {}).get('enable'), False)

    def test_gcp_split_host_disables_logging_for_web_and_api(self):
        backends = {d['metadata']['name']: d for d in yaml_documents(
            (CHART / 'examples/backendconfig-api-no-iap.yaml').read_text())}
        services = [d for d in render('values-iap.yaml.example')
                    if d and d['kind'] == 'Service' and
                    'cloud.google.com/backend-config' in d['metadata'].get('annotations', {})]
        referenced = set()
        for service in services:
            reference = json.loads(service['metadata']['annotations']['cloud.google.com/backend-config'])
            for name in reference.values():
                referenced.add(name)
                self.assertIs(backends[name]['spec'].get('logging', {}).get('enable'), False)
        self.assertEqual(referenced, {'engram-web-iap', 'engram-api-no-iap'})


if __name__ == '__main__':
    unittest.main()
