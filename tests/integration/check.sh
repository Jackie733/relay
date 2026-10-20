#!/bin/sh
set -eu
cd "$(dirname "$0")"
known_hosts=$(mktemp)
trap 'rm -f "$known_hosts"' EXIT HUP INT TERM

# These are disposable local test hosts, never user or production hosts.
for port in 2221 2222 2223; do
    ssh-keyscan -T 5 -p "$port" 127.0.0.1 >> "$known_hosts" 2>/dev/null
    result=$(ssh -F /dev/null -o BatchMode=yes -o IdentitiesOnly=yes \
        -o StrictHostKeyChecking=yes -o "UserKnownHostsFile=$known_hosts" \
        -o GlobalKnownHostsFile=/dev/null -o ConnectTimeout=5 \
        -i .fixtures/id_ed25519 -p "$port" relay@127.0.0.1 'printf relay-ok')
    test "$result" = relay-ok
    printf 'OpenSSH port %s: public-key authentication OK\n' "$port"
done

# Independent Docker networks must not provide container-to-container routing.
for target in openssh-b openssh-c; do
    if docker compose exec -T openssh-a getent hosts "$target"; then
        printf 'Unexpected cross-network DNS: %s\n' "$target" >&2
        exit 1
    fi
done
