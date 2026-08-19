-- Rename the in-progress run/job status. STARTING is still accepted when
-- reading old rows; new writes use SYNCING.
UPDATE job_runs SET status = 'SYNCING' WHERE status = 'STARTING';
UPDATE jobs SET status = 'SYNCING' WHERE status = 'STARTING';
