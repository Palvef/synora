# PyPI cache access records

The cache reads its seven-day package-request window from ClickHouse. It does
not scan Nginx access logs or fall back to local logs when ClickHouse fails.
Yukina still examines its package directory to manage the cache budget.

Mount a dedicated configuration file read-only and set
`PYPI_CLICKHOUSE_CONFIG=/run/secrets/pypi-clickhouse.json`:

```json
{
  "endpoint": "http://internal-clickhouse:8123",
  "username": "synora_pypi_cache",
  "password": "REPLACE_WITH_DEDICATED_PASSWORD",
  "table": "mirrorlog.synora_pypi_access",
  "max_age_seconds": 3600
}
```

The view must return `timestamp` (Unix seconds), `clientip` (IPv4/IPv6 text),
`url`, `size`, `status`, `user_agent`, and `proxied` (`"0"` or `"1"`). Restrict
it to your mirror's PyPI package requests and the last seven days. If the source
has no explicit cache-origin field, derive `proxied` from a nonempty upstream
address. Grant the cache account SELECT on this view only; use a definer view
when the account must not read the underlying access-log table.

Example extra job settings:

```toml
env = ["PYPI_CLICKHOUSE_CONFIG=/run/secrets/pypi-clickhouse.json"]
volumes = ["/var/lib/synora/secrets/pypi-clickhouse.json:/run/secrets/pypi-clickhouse.json:ro"]
```

Keep the host credentials file private and remove the old `/nginx-log` and
`/nginx-legacy` mounts. Permit the worker to reach the internal endpoint.

The runner streams JSONEachRow, validates every record, checks the returned
row count, and atomically replaces the vote file only after a complete query.
Empty, invalid, stale or incomplete results stop synchronization before cache
eviction. The latest valid package request must be newer than
`max_age_seconds`; this prevents stale ingestion data from authorizing eviction.
Query credentials are sent in an authorization header and never printed.
