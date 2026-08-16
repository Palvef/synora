-- Config audit (spec §86): every reload records what changed per job.

CREATE TABLE config_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    job_name TEXT NOT NULL,
    before_json TEXT,
    after_json TEXT
);
