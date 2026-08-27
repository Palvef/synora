#!/usr/bin/env python3
"""Restart a drained Synora worker only after all active runs finish."""

import argparse
import json
import subprocess
import time
import tomllib
import urllib.request


def worker_state(config_path: str) -> tuple[str, str, str]:
    with open(config_path, "rb") as handle:
        worker = tomllib.load(handle)["worker"]
    return worker["manager"].rstrip("/"), worker["token"], worker["name"]


def fetch_worker(manager: str, token: str, name: str) -> dict:
    request = urllib.request.Request(
        f"{manager}/api/v1/workers",
        headers={"Authorization": f"Bearer {token}"},
    )
    with urllib.request.urlopen(request, timeout=15) as response:
        workers = json.load(response)
    return next(worker for worker in workers if worker["id"] == name)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", default="/etc/synora/worker.toml")
    parser.add_argument("--service", default="synora-worker.service")
    parser.add_argument("--interval", type=int, default=30)
    args = parser.parse_args()

    manager, token, name = worker_state(args.config)
    while True:
        try:
            worker = fetch_worker(manager, token, name)
            running = int(worker.get("jobs_running", 0))
            status = str(worker.get("status", ""))
            print(f"worker={name} status={status} jobs_running={running}", flush=True)
            if status == "DRAINING" and running == 0:
                subprocess.run(["systemctl", "restart", args.service], check=True)
                print(f"restarted {args.service} after drain completed", flush=True)
                return
        except Exception as error:  # keep the watcher alive across manager restarts
            print(f"state check failed: {error}", flush=True)
        time.sleep(max(args.interval, 5))


if __name__ == "__main__":
    main()
