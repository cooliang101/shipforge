FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server curl python3 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --shell /bin/sh deploy \
    && passwd -d deploy \
    && mkdir -p /run/sshd /fixture /srv/shipforge-acceptance \
    && chown deploy:deploy /srv/shipforge-acceptance
COPY sshd_config /fixture/sshd_config
COPY entrypoint.sh /fixture/entrypoint.sh
COPY health.py /fixture/health.py
COPY marker /etc/shipforge-disposable-fixture
EXPOSE 22
ENTRYPOINT ["/bin/sh", "/fixture/entrypoint.sh"]
