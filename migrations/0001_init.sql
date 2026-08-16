-- Core schema (spec §27). Shared by SQLite and PostgreSQL:
-- TEXT primary keys, INTEGER 0/1 booleans, INTEGER unix-seconds timestamps (UTC).
-- `schema_migrations` itself is created by the migrator, not here.

CREATE TABLE jobs (
    name TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL DEFAULT 1,
    worker TEXT,                          -- explicit worker / worker group; NULL = auto
    provider TEXT NOT NULL,               -- rsync|script|docker
    provider_config TEXT NOT NULL,        -- JSON
    upstream TEXT,
    storage_path TEXT NOT NULL,
    timeout_secs INTEGER NOT NULL DEFAULT 7200,
    retry INTEGER NOT NULL DEFAULT 3,
    retry_delay_secs INTEGER NOT NULL DEFAULT 300,
    retry_backoff REAL NOT NULL DEFAULT 2.0,
    success_exit_codes TEXT NOT NULL DEFAULT '[]',  -- JSON array
    fail_on_match TEXT,
    max_concurrency INTEGER NOT NULL DEFAULT 1,
    on_worker_lost TEXT NOT NULL DEFAULT 'retry',
    statistics TEXT NOT NULL DEFAULT 'provider',
    resources TEXT NOT NULL DEFAULT '[]',  -- JSON array
    priority INTEGER NOT NULL DEFAULT 50,
    status TEXT NOT NULL DEFAULT 'PENDING',  -- denormalized mirror of current run
    last_run_at INTEGER,
    updated_at INTEGER NOT NULL
);

CREATE TABLE schedules (
    job_name TEXT PRIMARY KEY REFERENCES jobs(name),
    schedule_json TEXT NOT NULL,          -- serialized Schedule
    timezone TEXT NOT NULL DEFAULT 'UTC',
    misfire_policy TEXT NOT NULL DEFAULT 'skip',
    next_run INTEGER,                     -- unix seconds UTC
    anchor_at INTEGER,                    -- interval alignment anchor (no-drift)
    created_at INTEGER NOT NULL
);

CREATE TABLE workers (
    id TEXT PRIMARY KEY,
    hostname TEXT NOT NULL,
    address TEXT NOT NULL,
    version TEXT NOT NULL,
    labels TEXT NOT NULL DEFAULT '[]',    -- JSON array
    capabilities TEXT NOT NULL DEFAULT '{}', -- JSON object
    status TEXT NOT NULL DEFAULT 'ONLINE', -- ONLINE|OFFLINE|DRAINING|MAINTENANCE
    jobs_running INTEGER NOT NULL DEFAULT 0,
    last_heartbeat INTEGER NOT NULL,
    registered_at INTEGER NOT NULL
);

CREATE TABLE job_runs (
    id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL REFERENCES jobs(name),
    worker_id TEXT REFERENCES workers(id),
    status TEXT NOT NULL,                 -- QUEUED|STARTING|RUNNING|SUCCESS|FAILED|RETRYING|CANCELLING|CANCELLED|LOST
    retry_count INTEGER NOT NULL DEFAULT 0,
    lost_count INTEGER NOT NULL DEFAULT 0,
    started_at INTEGER,
    finished_at INTEGER,
    duration_secs INTEGER,
    exit_code INTEGER,
    size_before INTEGER,
    size_after INTEGER,
    bytes_transferred INTEGER,
    message TEXT,
    lease_expires_at INTEGER,
    next_retry_at INTEGER,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_runs_job ON job_runs(job_id, created_at DESC);
CREATE INDEX idx_runs_reap ON job_runs(status, lease_expires_at);
CREATE INDEX idx_runs_retry ON job_runs(status, next_retry_at);

CREATE TABLE repositories (
    path TEXT PRIMARY KEY,
    size_bytes INTEGER,
    last_measured_at INTEGER
);

CREATE TABLE events (
    id TEXT PRIMARY KEY,
    ts INTEGER NOT NULL,
    job_id TEXT,
    run_id TEXT,
    level TEXT NOT NULL,
    message TEXT NOT NULL
);

CREATE TABLE job_logs (
    run_id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL,
    log_path TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
