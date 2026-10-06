FROM rust:1.94-bookworm AS source
RUN apt-get update && apt-get install -y --no-install-recommends clang libclang-dev cmake python3 && rm -rf /var/lib/apt/lists/*
ARG YUKINA_REV=c111383aeb4bd8fb701f0fff060ec46780f84c2a
WORKDIR /src
RUN git init && git remote add origin https://github.com/taoky/yukina.git \
    && git fetch --depth=1 origin "$YUKINA_REV" && git checkout --detach FETCH_HEAD
COPY pypi-runtime/patch_upstream.py /tmp/patch_upstream.py
RUN python3 /tmp/patch_upstream.py && cargo build --locked --release --bin yukina

FROM python:3.13-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libclang1-14 && rm -rf /var/lib/apt/lists/*
COPY --from=source /src/target/release/yukina /usr/local/bin/yukina
COPY pypi-runtime/sync.py pypi-runtime/logs.py /opt/pypi-cache/
ENV PYTHONUNBUFFERED=1 PYPI_INDEX_MODE=proxy
ENTRYPOINT ["python", "/opt/pypi-cache/sync.py"]
WORKDIR /data
