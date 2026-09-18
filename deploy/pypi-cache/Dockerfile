# Dedicated, source-pinned index synchronizer and asynchronous package cache.
FROM rust:1.94-bookworm AS yukina
RUN apt-get update && apt-get install -y --no-install-recommends clang libclang-dev cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN git init && git remote add origin https://github.com/taoky/yukina.git && git -c http.version=HTTP/1.1 fetch --depth=1 origin c111383aeb4bd8fb701f0fff060ec46780f84c2a && git checkout --detach FETCH_HEAD
COPY patch_upstream.py /tmp/patch_upstream.py
RUN python3 /tmp/patch_upstream.py
RUN cargo build --locked --release --bin yukina

FROM python:3.13-slim-bookworm AS shadowmire
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN git init && git remote add origin https://github.com/taoky/shadowmire.git && git -c http.version=HTTP/1.1 fetch --depth=1 origin 537c01781a4fa07f4a48a1c2a5cc6604a637cc21 && git checkout --detach FETCH_HEAD
RUN python -m venv /opt/shadowmire && /opt/shadowmire/bin/pip install --no-cache-dir .

FROM python:3.13-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libclang1-14 && rm -rf /var/lib/apt/lists/*
COPY --from=yukina /src/target/release/yukina /usr/local/bin/yukina
COPY --from=shadowmire /opt/shadowmire /opt/shadowmire
ENV PATH="/opt/shadowmire/bin:${PATH}" PYTHONUNBUFFERED=1 SHADOWMIRE_WORKERS=16
COPY sync.py logs.py /opt/pypi-cache/
WORKDIR /data
ENTRYPOINT ["python", "/opt/pypi-cache/sync.py"]
