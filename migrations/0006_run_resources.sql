-- Persist last-run resource samples so Grafana can show CPU/memory for
-- every job, not only the ones currently executing.
ALTER TABLE job_runs ADD COLUMN memory_bytes INTEGER;
ALTER TABLE job_runs ADD COLUMN cpu_seconds REAL;
