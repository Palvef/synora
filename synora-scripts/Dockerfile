# General APT/YUM, git and Python synchronization jobs. Specialized tools have separate images.
ARG SOURCE_RUNTIME=synora-scripts:0.2.0
FROM ${SOURCE_RUNTIME} AS source
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
        zstd \
        rpm \
        awscli \
    && rm -rf /var/lib/apt/lists/*


COPY --from=source /usr/local/bin/repo /usr/local/bin/repo
RUN ln -s /usr/local/bin/repo /usr/local/bin/aosp-repo

COPY . /usr/lib/synora/scripts

RUN find /usr/lib/synora/scripts -type f \( -name '*.sh' -o -name '*.py' \) -exec chmod 0755 {} + \
    && ln -sfn /usr/lib/synora/scripts /home/tunasync-scripts \
    && git config --system --add safe.directory '*' \
    && git config --system user.email "mirrors@hernet" \
    && git config --system user.name "hernet mirrors"

WORKDIR /data
# No ENTRYPOINT. The worker runs `docker --init --entrypoint <script>` so
# git/repo children are reaped and proxy env is injected by Synora.
CMD ["/bin/bash"]
