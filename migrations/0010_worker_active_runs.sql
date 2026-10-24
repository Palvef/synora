-- Heartbeat/complete slot reconciliation must scale with one worker's active
-- assignments, rather than scanning another worker's runs or terminal history.
CREATE INDEX idx_runs_worker_active ON job_runs(worker_id, lease_expires_at)
WHERE status IN ('STARTING','SYNCING','RUNNING','CANCELLING');
