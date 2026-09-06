ALTER TABLE job_runs ADD COLUMN lease_token TEXT;
CREATE UNIQUE INDEX one_active_run_per_job ON job_runs(job_id) WHERE status IN ('STARTING','SYNCING','RUNNING','CANCELLING');
