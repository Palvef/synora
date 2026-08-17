-- Remote workers report their run log with `complete`; the manager stores
-- it here so job_logs works for distributed runs (worker logs live on the
-- worker host, not next to the manager).
ALTER TABLE job_logs ADD COLUMN content TEXT NOT NULL DEFAULT '';
