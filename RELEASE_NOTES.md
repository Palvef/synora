# Synora 0.2.1

## HTTP sync outcomes

- Missing ordinary files (HTTP 404/410) now complete with warnings, retaining
  missing-file counts and diagnostic paths without retrying the entire job.
- Missing critical repository metadata, incomplete directory traversal, other
  HTTP errors, network failures and filesystem errors still fail the run.
- Any missing-file warning or fatal error suppresses local deletion. Partial
  mirrors no longer claim the complete upstream size.
- API, CLI, TUI, database history and Grafana distinguish `success_with_warnings`.
  Completed warning runs satisfy dependencies; tunasync-compatible status maps
  them to success. The native status metric uses value 12.

## Proxy expose

- Authenticated HTTP upstream proxies support both regular HTTP and CONNECT
  tunnels through expose listeners, with independent downstream credentials.
- Proxy authentication works regardless of header order. Listener startup logs
  omit upstream credentials.

## Upgrading

- Upgrade the Manager before Workers so it accepts the new completion status.
  Drain running Workers before replacing their processes. No schema migration
  is needed from 0.2.0; historical failures are not rewritten.
- Re-import the Grafana dashboard to display completed-with-warning runs.
- Linux release builds retain Ubuntu 22.04 / glibc 2.35 compatibility checks.
