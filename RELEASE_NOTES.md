# Synora 0.1.6

## Fixes

- Docker jobs run with `--init` and an explicit `--entrypoint`, so git/repo
  children are reaped instead of becoming zombies under python PID 1
- Worker samples all `synora-job-*` containers with one waited `docker stats`
  instead of one CLI per job
- `synora job stop` force-removes the named container; killing `docker run`
  alone left the job running
- Disabled jobs no longer retry after a failed or cancelled run
- `provider = "git"` executes `git.sh` in the scripts image
- Git mirror scripts stop listing every ref (`git remote -v`) and no longer
  walk pack files with `find objects`

## Notes

- `http_connect_proxy.py` stays in the image for optional use; it is not PID 1
- rustup-mirror is still built in-image from jiegec/rustup-mirror
