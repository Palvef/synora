# synora-scripts

Mirror sync scripts for Synora `provider = "script"` jobs.

They run inside the `synora-scripts` image (`docker run synora-scripts:latest`).
Git jobs (`provider = "git"`) use the same image. Job commands stay
`/usr/lib/synora/scripts/<name>`.

## Environment

| Variable | Meaning |
|---|---|
| `SYNORA_JOB` | job name |
| `SYNORA_UPSTREAM` | upstream URL |
| `SYNORA_STORAGE` | working directory (bind-mounted host path) |
| `SYNORA_RUN_ID` | current run id |
| `SYNORA_PROXY` / `ALL_PROXY` / `HTTP(S)_PROXY` | assigned proxy |
| `SYNORA_SIZE=` | bytes, printed on stdout when known |
| `SYNORA_STATUS=success\|failed` | optional explicit outcome |

HTTP directory mirrors use Synora's native `http` provider, not `tsumugu.sh`.

## Image

```sh
scripts/build-synora-scripts-image.sh
```

Apt stays direct. Optional HTTPS fetch proxy for git/gem/pip/curl:

```sh
scripts/build-synora-scripts-image.sh --proxy "$HTTPS_PROXY"
```

The image includes git, ftpsync (archvsync), python3, dnf, createrepo_c,
awscli, `repo`, rubygems-mirror, and rustup-mirror.

## Acknowledgements

These scripts started as [tunasync-scripts](https://github.com/tuna/tunasync-scripts)
from TUNA (Tsinghua University TUNA Association). Synora rewrites them to
`SYNORA_*` environment variables and runs them in its own image. Thank you
to TUNA and the tunasync-scripts authors and maintainers.
