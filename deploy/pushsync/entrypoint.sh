#!/bin/sh
set -eu
mkdir -p /home/tunasync/.ssh /run/sshd
chmod 0755 /run/sshd
chmod 0700 /home/tunasync /home/tunasync/.ssh
if [ -f /home/tunasync/.ssh/authorized_keys ]; then
	chmod 0600 /home/tunasync/.ssh/authorized_keys
	chown tunasync:tunasync /home/tunasync/.ssh/authorized_keys
fi
chown tunasync:tunasync /home/tunasync /home/tunasync/.ssh
exec /usr/sbin/sshd -De
