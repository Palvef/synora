# General APT/YUM, git and Python synchronization jobs. Specialized tools have separate images.
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
        subversion \
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


ARG REPO_REV=4c37f588069d3a62b416ffcc25184119cd21e59e
RUN HTTPS_PROXY="$FETCH_HTTPS_PROXY" https_proxy="$FETCH_HTTPS_PROXY" \
    curl --fail --location --retry 5 --output /usr/local/bin/repo \
      "https://raw.githubusercontent.com/GerritCodeReview/git-repo/${REPO_REV}/repo" \
    && chmod 0755 /usr/local/bin/repo \
    && ln -s /usr/local/bin/repo /usr/local/bin/aosp-repo

# Source pinned to the helper used by tuna/tunasync-scripts/pybombs.sh.
ARG PYBOMBS_MIRROR_REV=d7a8a9925c0b91dc9695a31a07b62eff8546909e
RUN HTTPS_PROXY="$FETCH_HTTPS_PROXY" https_proxy="$FETCH_HTTPS_PROXY" \
    git clone https://github.com/scateu/pybombs-mirror.git /opt/pybombs-mirror \
    && git -C /opt/pybombs-mirror checkout --detach "$PYBOMBS_MIRROR_REV" \
    && rm -rf /opt/pybombs-mirror/.git

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
