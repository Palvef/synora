import json,os,subprocess,tempfile,unittest
from pathlib import Path

SCRIPT=Path(__file__).resolve().parents[1]/'nix.sh'

class NixCleanupTests(unittest.TestCase):
    def run_script(self, fail=False):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);storage=root/'storage';storage.mkdir();bin_dir=root/'bin';bin_dir.mkdir()
            log=root/'calls.jsonl';aws=bin_dir/'aws'
            aws.write_text('''#!/usr/bin/env python3
import json,os,sys
from pathlib import Path
log=Path(os.environ['AWS_CALL_LOG'])
count=len(log.read_text().splitlines()) if log.exists() else 0
with log.open('a') as output: output.write(json.dumps(sys.argv[1:])+'\\n')
if os.environ.get('FAIL_PAYLOAD')=='1' and count==0: sys.exit(1)
target=Path(sys.argv[-1]);(target/'nix-2.99.0').mkdir(parents=True,exist_ok=True)
if count==1: (target/'nix-2.99.0/install').write_text('installer')
''')
            aws.chmod(0o755)
            env=dict(os.environ,PATH=str(bin_dir)+':'+os.environ['PATH'],AWS_CALL_LOG=str(log),FAIL_PAYLOAD=str(int(fail)),SYNORA_STORAGE=str(storage),SYNORA_UPSTREAM='s3://example/nix/')
            result=subprocess.run(['bash',str(SCRIPT)],env=env,cwd=storage,capture_output=True)
            calls=[json.loads(line) for line in log.read_text().splitlines()]
            return result,calls
    def test_cleanup_follows_successful_payloads_and_protects_latest(self):
        result,calls=self.run_script()
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(len(calls),3)
        self.assertNotIn('--delete',calls[0]);self.assertNotIn('--delete',calls[1])
        self.assertIn('--delete',calls[2]);self.assertIn('latest/*',calls[2])
    def test_failed_payload_download_never_starts_cleanup(self):
        result,calls=self.run_script(True)
        self.assertNotEqual(result.returncode,0)
        self.assertEqual(len(calls),1)
        self.assertNotIn('--delete',calls[0])
