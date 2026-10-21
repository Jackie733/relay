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

# Check both DNS and direct TCP access, in every direction. First prove the
# TCP probe works against each container's own SSH listener.
for source in openssh-a openssh-b openssh-c; do
    docker compose exec -T "$source" timeout 3 bash -c \
        'exec 3<>/dev/tcp/127.0.0.1/22'
    for target in openssh-a openssh-b openssh-c; do
        [ "$source" != "$target" ] || continue
        if docker compose exec -T "$source" getent hosts "$target"; then
            printf 'Unexpected cross-network DNS: %s -> %s\n' "$source" "$target" >&2
            exit 1
        fi
        container=$(docker compose ps -q "$target")
        target_ip=$(docker inspect --format \
            '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$container")
        test -n "$target_ip"
        if docker compose exec -T "$source" timeout 3 bash -c \
            'exec 3<>/dev/tcp/"$1"/22' relay-probe "$target_ip" 2>/dev/null; then
            printf 'Unexpected cross-network SSH access: %s -> %s (%s)\n' \
                "$source" "$target" "$target_ip" >&2
            exit 1
        else
            status=$?
            case "$status" in
                1|124) ;; # Connection rejected/unreachable, or timed out.
                *) printf 'TCP probe failed unexpectedly: %s\n' "$status" >&2; exit 1 ;;
            esac
        fi
        printf 'Isolation OK: %s -> %s (DNS and TCP port 22 blocked)\n' "$source" "$target"
    done
done
