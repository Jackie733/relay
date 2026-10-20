FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server openssh-client \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /bin/bash relay \
    && passwd -d relay \
    && mkdir -p /run/sshd /etc/relay/keys \
    && rm -f /etc/ssh/ssh_host_*

COPY sshd_config /etc/relay/sshd_config
COPY entrypoint.sh /usr/local/bin/relay-test-sshd
EXPOSE 22
ENTRYPOINT ["sh", "/usr/local/bin/relay-test-sshd"]
