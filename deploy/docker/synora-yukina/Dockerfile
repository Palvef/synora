# Build with the existing, source-pinned PyPI cache runtime as an artifact source.
# It supplies the production-patched binary and runner; no Shadowmire is copied.
ARG PYPI_RUNTIME=synora-pypi:latest
FROM ${PYPI_RUNTIME} AS source
FROM python:3.13-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libclang1-14 && rm -rf /var/lib/apt/lists/*
COPY --from=source /usr/local/bin/yukina /usr/local/bin/yukina
COPY --from=source /opt/pypi-cache /opt/pypi-cache
ENV PYTHONUNBUFFERED=1 PYPI_INDEX_MODE=proxy
ENTRYPOINT ["python", "/opt/pypi-cache/sync.py"]

WORKDIR /data
