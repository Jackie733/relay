#!/bin/sh
set -eu
if [ ! -f /etc/relay/keys/ssh_host_ed25519_key ]; then
    ssh-keygen -q -t ed25519 -N '' -f /etc/relay/keys/ssh_host_ed25519_key
fi
exec /usr/sbin/sshd -D -e -f /etc/relay/sshd_config
