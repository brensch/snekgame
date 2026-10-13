#!/bin/sh
# One-time (idempotent) setup of the schlift.com VM for snekgame; `make setup-vm`
# runs it there as root after copying snekgame.service and snek.caddyfile to /tmp.
# Afterwards `make deploy` installs the binary. Data lives in /opt/snekgame
# (snek.db); the WebTransport cert is Caddy's, so snek.brensch.com must point at
# the VM and Caddy must have its certificate before the service can start.
set -eu
id snek >/dev/null 2>&1 || useradd --system --home-dir /opt/snekgame --shell /sbin/nologin snek
install -d -o snek -g snek -m 755 /opt/snekgame
install -d -o snek -g snek -m 700 /opt/snekgame/certs
install -m 644 /tmp/snekgame.service /etc/systemd/system/snekgame.service
install -d -m 755 /etc/caddy/Caddyfile.d
install -m 644 /tmp/snek.caddyfile /etc/caddy/Caddyfile.d/snek.caddyfile
rm -f /tmp/snekgame.service /tmp/snek.caddyfile
# WebTransport is QUIC on UDP 443 (Caddy is kept off UDP with `protocols h1 h2`).
firewall-cmd -q --permanent --add-port=443/udp && firewall-cmd -q --reload
out=$(caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile 2>&1) || { echo "$out"; exit 1; }
systemctl reload caddy
systemctl daemon-reload
systemctl enable snekgame >/dev/null 2>&1
echo "snekgame set up; run make deploy"
