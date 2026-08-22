FROM debian:trixie

# Host network SSH on port 22222. User remains tunasync.
# authorized_keys and synora.env are bind-mounted at runtime.

RUN apt-get update \
 && apt-get upgrade -y \
 && apt-get install -y --no-install-recommends tzdata openssh-server curl ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /run/sshd /home/tunasync/.ssh \
 && chmod 0755 /run/sshd \
 && ssh-keygen -A \
 && adduser --home /home/tunasync --shell /home/tunasync/pushsync.sh --disabled-password --gecos "" tunasync \
 && chmod 0700 /home/tunasync/.ssh

COPY --chown=tunasync:tunasync pushsync.sh key_repo_map.conf /home/tunasync/
COPY disable_password.conf /etc/ssh/sshd_config.d/disable_password.conf
COPY entrypoint.sh /entrypoint.sh
RUN chmod 0755 /home/tunasync/pushsync.sh /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
