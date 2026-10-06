FROM rust:1.94-bookworm AS source
ARG RUSTUP_MIRROR_REV=df47bd0a6527b7781e6ec5a2136e4cc1960c0030
COPY patches/rustup-mirror-large-file-timeout.patch /tmp/rustup-mirror.patch
WORKDIR /src
RUN git init && git remote add origin https://github.com/jiegec/rustup-mirror.git \
    && git fetch --depth=1 origin "$RUSTUP_MIRROR_REV" && git checkout --detach FETCH_HEAD \
    && git apply /tmp/rustup-mirror.patch && cargo build --locked --release
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends libssl3t64 python3 python3-requests python3-socks ca-certificates curl bash && rm -rf /var/lib/apt/lists/*
COPY --from=source /src/target/release/rustup-mirror /usr/local/cargo/bin/rustup-mirror
COPY rustup.sh rustup-official-proxy.py /usr/lib/synora/scripts/
COPY helpers/http_connect_proxy.py /usr/lib/synora/scripts/helpers/http_connect_proxy.py

WORKDIR /data
ENV PATH="/ftpsync/bin:/usr/local/cargo/bin:/usr/lib/synora/scripts:${PATH}" LANG=C.UTF-8
RUN chmod -R a+rX /usr/lib/synora/scripts && find /usr/lib/synora/scripts -type f -exec chmod 0755 {} + && ln -s /usr/lib/synora/scripts /home/tunasync-scripts
CMD ["/bin/bash"]
