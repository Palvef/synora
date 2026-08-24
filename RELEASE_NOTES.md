# Synora 0.1.8

## Fixes

- Job-level proxy routing no longer inherits the worker's general-purpose
  default proxy unless the job explicitly selects one
- The manager's exposed SOCKS-backed proxy now forwards plain HTTP
  absolute-form requests as well as HTTPS `CONNECT` requests
- HTTP directory mirrors use a 30-second connect timeout and a 120-second
  idle-read timeout, accept up to one million listing entries, and report
  failed files directly in job logs
- Proxmox and VirtualBox scripts support HTTP-over-CONNECT proxies on older
  managers through the bundled compatibility helper
- Rustup mirrors allow long large-file transfers; the in-image mirror is
  pinned and patched with an environment-configurable timeout (six hours by
  default)

## Notes

- Rebuild `synora-scripts:latest` on workers to apply the script and rustup
  mirror fixes
