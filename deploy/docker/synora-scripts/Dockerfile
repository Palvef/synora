# Synora git/script runtime. Job configs stay provider = "git" / "script";
# the worker runs them with `docker run synora-scripts:latest`.
# HTTP directory mirrors use the native http provider (former tsumugu).
FROM debian:trixie-slim

# Used only for HTTPS fetches (git clone / gem / pip / repo). Do not set
# HTTP_PROXY here: the manager CONNECT expose answers GET with 405, which
# breaks apt's http://deb.debian.org requests.
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
    && chmod 0777 /var/log/ftpsync

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

COPY scripts /usr/lib/synora/scripts
COPY rustup-mirror /usr/local/cargo/bin/rustup-mirror

RUN chmod 0755 /usr/local/cargo/bin/rustup-mirror \
    && find /usr/lib/synora/scripts -type f \( -name '*.sh' -o -name '*.py' \) -exec chmod 0755 {} + \
    && ln -sfn /usr/lib/synora/scripts /home/tunasync-scripts \
    && ln -sfn /usr/local/cargo/bin/rustup-mirror /usr/lib/synora/scripts/rustup-mirror \
    && git config --system --add safe.directory '*' \
    && git config --system user.email "mirrors@synora" \
    && git config --system user.name "synora mirrors"

WORKDIR /data
CMD ["/bin/bash"]
