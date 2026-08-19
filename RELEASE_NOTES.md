# Synora 0.1.3

## Fixes

- HTTP listings that fail, truncate, or hit depth/entry caps no longer delete local files
- HTTP `size_hint` is the remote repository size, not just this run's downloads
- Unchanged HTTP files are compared by size first; mtime is only a fallback
- Worker re-register re-queues lost runs when `on_worker_lost = retry`
- A live worker heartbeat keeps its runs from being marked lost
- HTTP directory listings retry transient proxy/connection errors
- Workers retry `complete_run` so a manager restart does not drop the outcome
- Grafana success rate uses live counters so it no longer sticks at 0 after restart
- Workers poll every 2s while under their concurrency cap so slots fill promptly

## Improvements

- Grafana dashboard rewritten: per-job CPU/memory, repository sizes, and clearer status tables
- All providers report CPU and memory samples
- Docker jobs inject `ALL_PROXY`/`all_proxy` and no longer inherit host `http_proxy`
- Deleting a job removes its leftover rows from the database
- `retire` replaces `drain` for stopping a worker from taking new runs (`drain` remains an alias)

