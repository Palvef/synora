ARG SOURCE_RUNTIME=synora-scripts:0.2.0
FROM ${SOURCE_RUNTIME} AS source
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends bash rsync ca-certificates openssh-client coreutils findutils gawk grep sed hostname procps curl && rm -rf /var/lib/apt/lists/*
COPY --from=source /ftpsync /ftpsync
RUN mkdir -p /var/log/ftpsync && chmod 0777 /var/log/ftpsync && printf '#!/bin/sh\nexit 0\n' > /usr/sbin/sendmail && chmod 0755 /usr/sbin/sendmail && ln -s /ftpsync/bin/ftpsync /usr/local/bin/ftpsync

COPY debian.sh kali.sh /usr/lib/synora/scripts/
RUN chmod 0755 /usr/lib/synora/scripts/*.sh && ln -s /usr/lib/synora/scripts /home/tunasync-scripts
WORKDIR /data
ENV PATH="/ftpsync/bin:/usr/local/cargo/bin:/usr/lib/synora/scripts:${PATH}" LANG=C.UTF-8
CMD ["/bin/bash"]
