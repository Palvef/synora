import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import tunasync2synora  # noqa: E402
import yuki2synora  # noqa: E402


class TunasyncMigrationTests(unittest.TestCase):
    def test_rsync_globals_and_integer_exit_codes_are_preserved(self):
        text = tunasync2synora.render_job(
            {
                "name": "debian",
                "provider": "rsync",
                "upstream": "rsync://example/debian/",
                "mirror_dir": "/srv/custom/debian",
                "rsync_options": ["--delete-excluded"],
                "success_exit_codes": [24],
                "rsync_success_exit_codes": [25],
                "use_ipv6": True,
            },
            {
                "interval": 60,
                "retry": 2,
                "timeout": 7200,
                "rsync_options": ["--numeric-ids"],
                "dangerous_global_success_exit_codes": [10],
                "dangerous_global_rsync_success_exit_codes": [23],
            },
            {},
        )
        job = tomllib.loads(text)["jobs"][0]
        self.assertEqual(job["success_exit_codes"], [10, 24, 23, 25])
        self.assertEqual(job["options"], ["--numeric-ids", "--delete-excluded"])
        self.assertEqual(job["storage"], "/srv/custom/debian")
        self.assertNotIn("storage_name", job)
        self.assertEqual(job["family"], "ipv6")
        # tunasync retry=2 means two total attempts; Synora retry=1 means one
        # retry after the initial attempt.
        self.assertEqual(job["retry"], 1)
        self.assertEqual(job["timeout"], 7200)

    def test_missing_retry_preserves_tunasync_default_attempt_count(self):
        text = tunasync2synora.render_job(
            {
                "name": "default-retry",
                "provider": "rsync",
                "upstream": "rsync://example/default-retry/",
            },
            {},
            {},
        )
        job = tomllib.loads(text)["jobs"][0]
        self.assertEqual(job["retry"], 1)

    def test_include_files_are_loaded_as_toml(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "mirrors").mkdir()
            (root / "workers.conf").write_text(
                '[global]\nmirror_dir = "/srv/mirror"\n\n'
                '[include]\ninclude_mirrors = "mirrors/*.conf"\n',
                encoding="utf-8",
            )
            (root / "mirrors" / "one.conf").write_text(
                '[[mirrors]]\nname = "one"\nprovider = "command"\n'
                'command = "echo # is data"\nenv = { TOKEN = "a#b" }\n',
                encoding="utf-8",
            )
            config, mirrors = tunasync2synora.load_tunasync(root / "workers.conf")
            self.assertIn("global", config)
            self.assertEqual(len(mirrors), 1)
            self.assertEqual(mirrors[0]["env"]["TOKEN"], "a#b")

    def test_docker_image_and_volumes_are_not_replaced(self):
        text = tunasync2synora.render_job(
            {
                "name": "custom",
                "provider": "command",
                "command": "/home/tunasync-scripts/sync.sh --fast",
                "docker_image": "example/mirror:v2",
                "docker_volumes": ["/keys:/keys:ro"],
                "docker_options": ["--cpus", "20"],
                "env": {"TOKEN": "a#b"},
            },
            {},
            {"volumes": ["/scripts:/scripts:ro"]},
        )
        job = tomllib.loads(text)["jobs"][0]
        self.assertEqual(job["image"], "example/mirror:v2")
        self.assertEqual(job["docker_options"], ["--cpus", "20"])
        self.assertEqual(
            job["docker_command"], ["/home/tunasync-scripts/sync.sh", "--fast"]
        )
        self.assertEqual(
            job["volumes"], ["/scripts:/scripts:ro", "/keys:/keys:ro"]
        )
        self.assertEqual(job["env"], ["TOKEN=a#b"])


class YukiMigrationTests(unittest.TestCase):
    def test_runtime_env_volumes_network_and_hashes_are_preserved(self):
        repo = yuki2synora.parse_yaml_simple(
            '''name: docker-ce
cron: "0 * * * *"
storageDir: /srv/docker-ce
image: "ustcmirror/rsync:latest"
user: "1000:1000"
bindIP: 192.0.2.10
network: mirror-net
retry: 4
logRotCycle: 3
envs:
  TOKEN: "abc#123" # actual comment
volumes:
  /srv/keys: /keys
'''
        )
        job = tomllib.loads(yuki2synora.render_job(repo))["jobs"][0]
        self.assertEqual(job["docker_network"], "mirror-net")
        self.assertEqual(
            job["volumes"],
            ["/var/log/synora/docker-ce:/log", "/srv/keys:/keys"],
        )
        self.assertEqual(job["retry"], 0)
        self.assertIn("TOKEN=abc#123", job["env"])
        self.assertIn("REPO=docker-ce", job["env"])
        self.assertIn("OWNER=1000:1000", job["env"])
        self.assertIn("BIND_ADDRESS=192.0.2.10", job["env"])
        self.assertIn("RETRY=4", job["env"])
        self.assertIn("LOG_ROTATE_CYCLE=3", job["env"])

    def test_yuki_every_and_default_host_network_are_preserved(self):
        job = tomllib.loads(
            yuki2synora.render_job(
                {
                    "name": "hourly",
                    "cron": "@every 1h30m",
                    "storageDir": "/srv/hourly",
                    "image": "example/hourly:latest",
                },
                sync_timeout="48h",
            )
        )["jobs"][0]
        self.assertEqual(job["schedule"], "interval")
        self.assertEqual(job["every"], "1h30m")
        self.assertEqual(job["docker_network"], "host")
        self.assertEqual(job["timeout"], "48h")


if __name__ == "__main__":
    unittest.main()
