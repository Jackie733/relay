#!/bin/sh
set -eu
# Bind-mounted keys can belong to a different host UID (for example on CI).
# OpenSSH StrictModes requires authorized_keys to belong to root or relay.
install -d -m 700 -o relay -g relay /home/relay/.ssh
install -m 600 -o relay -g relay /etc/relay/authorized_keys /home/relay/.ssh/authorized_keys
if [ ! -f /etc/relay/keys/ssh_host_ed25519_key ]; then
    ssh-keygen -q -t ed25519 -N '' -f /etc/relay/keys/ssh_host_ed25519_key
fi
exec /usr/sbin/sshd -D -e -f /etc/relay/sshd_config
