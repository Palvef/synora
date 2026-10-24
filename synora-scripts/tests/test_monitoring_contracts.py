"""Release and dashboard contracts that affect production operators."""
import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]


class MonitoringContracts(unittest.TestCase):
    def test_current_resources_never_read_historical_peaks(self):
        dashboard = json.loads((ROOT/'deploy/grafana-synora.json').read_text())['dashboard']
        self.assertEqual(dashboard['uid'], 'synora-mirror')
        self.assertEqual(dashboard['refresh'], '5s')
        table = next(p for p in dashboard['panels'] if p.get('id') == 50)
        for target in table['targets']:
            if any(metric in target['expr'] for metric in ['synora_job_memory_bytes', 'synora_job_cpu_percent', 'synora_job_cpu_seconds', 'synora_job_bandwidth_bytes']):
                self.assertNotIn('last_over_time', target['expr'])
                self.assertIn('synora_job_status', target['expr'])
        running = next(p for p in dashboard['panels'] if p.get('id') == 30)
        elapsed = next(t['expr'] for t in running['targets'] if t['refId'] == 'F')
        self.assertIn('time()', elapsed)
        self.assertIn('synora_job_last_start_timestamp', elapsed)

    def test_release_baseline_is_ubuntu_2404(self):
        workflow = (ROOT/'.github/workflows/release.yml').read_text()
        self.assertIn('runs-on: ubuntu-24.04', workflow)
        self.assertNotIn('ubuntu-22.04', workflow)
        self.assertIn('LIMIT = (2, 39)', (ROOT/'scripts/check-release-glibc.py').read_text())
