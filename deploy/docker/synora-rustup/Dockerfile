ARG SOURCE_RUNTIME=synora-scripts:0.2.0
FROM ${SOURCE_RUNTIME} AS source
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-requests python3-socks ca-certificates curl bash && rm -rf /var/lib/apt/lists/*
COPY --from=source /usr/local/cargo/bin/rustup-mirror /usr/local/cargo/bin/rustup-mirror
COPY rustup.sh rustup-official-proxy.py /usr/lib/synora/scripts/
COPY helpers/http_connect_proxy.py /usr/lib/synora/scripts/helpers/http_connect_proxy.py

WORKDIR /data
ENV PATH="/ftpsync/bin:/usr/local/cargo/bin:/usr/lib/synora/scripts:${PATH}" LANG=C.UTF-8
RUN chmod -R a+rX /usr/lib/synora/scripts && find /usr/lib/synora/scripts -type f -exec chmod 0755 {} + && ln -s /usr/lib/synora/scripts /home/tunasync-scripts
CMD ["/bin/bash"]
