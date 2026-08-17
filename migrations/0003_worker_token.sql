-- Worker identity: bind each registered worker id to the token name that
-- registered it. claim/complete/heartbeat/unregister verify against this
-- binding (a token can only act on workers it registered).
ALTER TABLE workers ADD COLUMN token_name TEXT NOT NULL DEFAULT '';
