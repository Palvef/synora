FROM python:3.13-slim-bookworm AS source
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates && rm -rf /var/lib/apt/lists/*
ARG SHADOWMIRE_REV=537c01781a4fa07f4a48a1c2a5cc6604a637cc21
WORKDIR /src
RUN git init && git remote add origin https://github.com/taoky/shadowmire.git \
    && git fetch --depth=1 origin "$SHADOWMIRE_REV" && git checkout --detach FETCH_HEAD \
    && python -m venv /opt/shadowmire && /opt/shadowmire/bin/pip install --no-cache-dir .

FROM python:3.13-slim-bookworm
COPY --from=source /opt/shadowmire /opt/shadowmire
ENV PATH="/opt/shadowmire/bin:${PATH}" PYTHONUNBUFFERED=1
ENTRYPOINT ["shadowmire"]
WORKDIR /data
