ALTER TABLE jobs ADD COLUMN config_generation TEXT NOT NULL DEFAULT '';
ALTER TABLE job_runs ADD COLUMN config_generation TEXT NOT NULL DEFAULT '';
