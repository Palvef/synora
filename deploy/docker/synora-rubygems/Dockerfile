FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ruby ca-certificates curl bash && rm -rf /var/lib/apt/lists/*
ARG FETCH_HTTPS_PROXY
RUN HTTPS_PROXY="$FETCH_HTTPS_PROXY" https_proxy="$FETCH_HTTPS_PROXY" gem install --no-document rubygems-mirror
COPY rubygems.sh /usr/lib/synora/scripts/rubygems.sh

WORKDIR /data
ENV PATH="/ftpsync/bin:/usr/local/cargo/bin:/usr/lib/synora/scripts:${PATH}" LANG=C.UTF-8
RUN chmod -R a+rX /usr/lib/synora/scripts && find /usr/lib/synora/scripts -type f -exec chmod 0755 {} + && ln -s /usr/lib/synora/scripts /home/tunasync-scripts
CMD ["/bin/bash"]
