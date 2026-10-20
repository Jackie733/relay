#!/bin/sh
set -eu
cd "$(dirname "$0")"
umask 077
mkdir -p .fixtures
if [ ! -f .fixtures/id_ed25519 ]; then
    ssh-keygen -q -t ed25519 -N '' -C relay-integration-only -f .fixtures/id_ed25519
fi
# Public key is mounted into test containers; no private key enters an image.
chmod 644 .fixtures/id_ed25519.pub
