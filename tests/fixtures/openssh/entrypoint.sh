#!/bin/sh
set -eu
mkdir -p /home/deploy/.ssh
cp /fixture/authorized_keys /home/deploy/.ssh/authorized_keys
chmod 700 /home/deploy/.ssh
chmod 600 /home/deploy/.ssh/authorized_keys
chown -R deploy:deploy /home/deploy/.ssh
ssh-keygen -q -t ed25519 -N '' -f /fixture/host_ed25519
python3 /fixture/health.py &
exec /usr/sbin/sshd -D -e -f /fixture/sshd_config
