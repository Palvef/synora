FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-requests python3-socks python3-tz python3-pyquery python3-pip awscli ca-certificates curl bash xz-utils && rm -rf /var/lib/apt/lists/*
ARG FETCH_HTTPS_PROXY
RUN HTTPS_PROXY="$FETCH_HTTPS_PROXY" https_proxy="$FETCH_HTTPS_PROXY" pip3 install --break-system-packages --no-cache-dir minio
COPY nix.sh nix-channels.py nixos-images.py /usr/lib/synora/scripts/

WORKDIR /data
ENV PATH="/ftpsync/bin:/usr/local/cargo/bin:/usr/lib/synora/scripts:${PATH}" LANG=C.UTF-8
RUN chmod -R a+rX /usr/lib/synora/scripts && find /usr/lib/synora/scripts -type f -exec chmod 0755 {} + && ln -s /usr/lib/synora/scripts /home/tunasync-scripts
CMD ["/bin/bash"]
