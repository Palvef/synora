# Synora 0.1.5

## Fixes

- Docker jobs no longer await `docker stats` on the log-drain task
- `docker stats` is killed after 2s so a hung Docker API cannot freeze stdout
- Stuck containers were blocking on a full pipe after stats hung for hours
