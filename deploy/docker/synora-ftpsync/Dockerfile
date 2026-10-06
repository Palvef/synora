FROM debian:trixie-slim AS source
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates && rm -rf /var/lib/apt/lists/*
ARG ARCHVSYNC_REV=653357779c338863917aa069afbae1b24472d32d
WORKDIR /ftpsync
RUN git init && git remote add origin https://salsa.debian.org/mirror-team/archvsync.git \
    && git fetch --depth=1 origin "$ARCHVSYNC_REV" && git checkout --detach FETCH_HEAD \
    && rm -rf .git
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends bash rsync ca-certificates openssh-client coreutils findutils gawk grep sed hostname procps curl && rm -rf /var/lib/apt/lists/*
COPY --from=source /ftpsync /ftpsync
RUN mkdir -p /var/log/ftpsync && chmod 0777 /var/log/ftpsync && printf '#!/bin/sh\nexit 0\n' > /usr/sbin/sendmail && chmod 0755 /usr/sbin/sendmail && ln -s /ftpsync/bin/ftpsync /usr/local/bin/ftpsync

COPY debian.sh kali.sh /usr/lib/synora/scripts/
COPY helpers/package-excludes.txt /usr/lib/synora/scripts/helpers/
RUN chmod 0755 /usr/lib/synora/scripts/*.sh && ln -s /usr/lib/synora/scripts /home/tunasync-scripts
WORKDIR /data
ENV PATH="/ftpsync/bin:/usr/local/cargo/bin:/usr/lib/synora/scripts:${PATH}" LANG=C.UTF-8
CMD ["/bin/bash"]
