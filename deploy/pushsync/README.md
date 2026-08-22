# pushsync

SSH forced-push endpoint for Synora jobs. Upstream mirrors log in as
`tunasync` on port 22222; `authorized_keys` `KEY_ID` selects allowed jobs.

Host files (not in git): `authorized_keys`, `synora.env`.

```sh
sudo docker run -itd \
  -v /path/to/pushsync/authorized_keys:/home/tunasync/.ssh/authorized_keys \
  -v /path/to/pushsync/disable_password.conf:/etc/ssh/sshd_config.d/disable_password.conf \
  -v /path/to/pushsync/key_repo_map.conf:/home/tunasync/key_repo_map.conf \
  -v /path/to/pushsync/pushsync.sh:/home/tunasync/pushsync.sh \
  -v /path/to/pushsync/synora.env:/home/tunasync/synora.env:ro \
  -v /path/to/pushsync/entrypoint.sh:/entrypoint.sh:ro \
  --restart always --name pushsync --net host pushsync
```

`synora.env` must be reachable from this container. Host network
can use loopback; otherwise use the manager's LAN/host URL, not
`127.0.0.1`. Job containers get `SYNORA_API` from `[worker].manager`.

```
SYNORA_API=http://MANAGER_HOST:9290
SYNORA_TOKEN=...
```

`~/.ssh` must be mode `0700` or sshd ignores `authorized_keys` (StrictModes).
The entrypoint enforces this on every start.
