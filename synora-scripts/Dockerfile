# synora-scripts runtime for git/script/docker jobs.
#
#   docker build -t synora-scripts:latest synora-scripts
#   scripts/build-synora-scripts-image.sh [--proxy URL]
#
# rustup-mirror is built from https://github.com/jiegec/rustup-mirror so the
# binary is not stored in git. HTTP directory mirrors use the native http
# provider (former tsumugu). FETCH_HTTPS_PROXY is only for HTTPS fetches
# (git/cargo/gem/pip/curl). Apt intentionally stays on its direct build path.

FROM rust:1-bookworm AS rustup-mirror
ARG FETCH_HTTPS_PROXY
ARG RUSTUP_MIRROR_REV=df47bd0a6527b7781e6ec5a2136e4cc1960c0030
WORKDIR /src
COPY patches/rustup-mirror-large-file-timeout.patch /tmp/rustup-mirror-large-file-timeout.patch
RUN set -e; \
    if [ -n "$FETCH_HTTPS_PROXY" ]; then \
      export HTTPS_PROXY="$FETCH_HTTPS_PROXY" \
             https_proxy="$FETCH_HTTPS_PROXY" \
             ALL_PROXY="$FETCH_HTTPS_PROXY" \
             all_proxy="$FETCH_HTTPS_PROXY"; \
    fi; \
    git init .; \
    git remote add origin https://github.com/jiegec/rustup-mirror.git; \
    git fetch --depth 1 origin "$RUSTUP_MIRROR_REV"; \
    git checkout --detach FETCH_HEAD; \
    git apply /tmp/rustup-mirror-large-file-timeout.patch; \
    cargo build --release --locked; \
    install -m0755 target/release/rustup-mirror /rustup-mirror

FROM debian:trixie-slim
ARG FETCH_HTTPS_PROXY

ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    PATH="/ftpsync/bin:/usr/local/cargo/bin:/usr/lib/synora/scripts:/usr/local/bin:${PATH}"

RUN apt-get update && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        coreutils \
        curl \
        wget \
        git \
        rsync \
        lftp \
        findutils \
        gawk \
        grep \
        sed \
        jq \
        unzip \
        tar \
        gzip \
        bzip2 \
        xz-utils \
        gnupg \
        procps \
        iproute2 \
        openssh-client \
        stunnel4 \
        socat \
        opensysusers \
        python3 \
        python-is-python3 \
        python3-pip \
        python3-requests \
        python3-yaml \
        python3-lxml \
        python3-pyquery \
        python3-tz \
        python3-click \
        python3-tqdm \
        python3-socks \
        python3-boto3 \
        python3-html5lib \
        python3-bs4 \
        dnf \
        dnf-plugins-core \
        createrepo-c \
        rpm \
        awscli \
        ruby \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /var/log/ftpsync \
    && chmod 0777 /var/log/ftpsync \
    && printf '#!/bin/sh\nexit 0\n' > /usr/sbin/sendmail \
    && chmod 0755 /usr/sbin/sendmail

RUN set -e; \
    if [ -n "$FETCH_HTTPS_PROXY" ]; then \
      export HTTPS_PROXY="$FETCH_HTTPS_PROXY" \
             https_proxy="$FETCH_HTTPS_PROXY" \
             ALL_PROXY="$FETCH_HTTPS_PROXY" \
             all_proxy="$FETCH_HTTPS_PROXY"; \
    fi; \
    git clone --depth 1 https://ftp-master.debian.org/git/archvsync.git /ftpsync; \
    ln -sfn /ftpsync/bin/ftpsync /usr/local/bin/ftpsync; \
    gem install --no-document rubygems-mirror; \
    curl -fsSL -o /usr/local/bin/repo https://storage.googleapis.com/git-repo-downloads/repo; \
    chmod 0755 /usr/local/bin/repo; \
    ln -sfn /usr/local/bin/repo /usr/local/bin/aosp-repo; \
    pip3 install --break-system-packages --no-cache-dir minio

COPY --from=rustup-mirror /rustup-mirror /usr/local/cargo/bin/rustup-mirror
COPY . /usr/lib/synora/scripts

RUN chmod 0755 /usr/local/cargo/bin/rustup-mirror \
    && find /usr/lib/synora/scripts -type f \( -name '*.sh' -o -name '*.py' \) -exec chmod 0755 {} + \
    && ln -sfn /usr/lib/synora/scripts /home/tunasync-scripts \
    && ln -sfn /usr/local/cargo/bin/rustup-mirror /usr/lib/synora/scripts/rustup-mirror \
    && git config --system --add safe.directory '*' \
    && git config --system user.email "mirrors@hernet" \
    && git config --system user.name "hernet mirrors"

WORKDIR /data
# No ENTRYPOINT. The worker runs `docker --init --entrypoint <script>` so
# git/repo children are reaped and proxy env is injected by Synora.
CMD ["/bin/bash"]
