ARG PYPI_RUNTIME=synora-pypi:latest
FROM ${PYPI_RUNTIME} AS source
FROM python:3.13-slim-bookworm
COPY --from=source /opt/shadowmire /opt/shadowmire
ENV PATH="/opt/shadowmire/bin:${PATH}" PYTHONUNBUFFERED=1
ENTRYPOINT ["shadowmire"]

WORKDIR /data
