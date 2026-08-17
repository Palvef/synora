-- Manual (forced) runs jump the dispatch queue: priority 1 beats the
-- scheduled backlog (priority 0). Workers pick by priority first, then
-- by creation order.
ALTER TABLE job_runs ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS idx_job_runs_priority ON job_runs (status, priority, created_at);
