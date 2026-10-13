# Build and ship the server to the schlift.com VM (ARM64, Oracle Linux).
#   make build     cross-build deploy/out/snek-server in Docker (no local toolchain)
#   make deploy    build, copy to the VM, restart the service
#   make setup-vm  one-time VM setup (user, systemd unit, Caddy site, firewall)
VM ?= opc@146.235.203.8

.PHONY: build deploy setup-vm
build:
	DOCKER_BUILDKIT=1 docker build -f deploy/Dockerfile --target export -o deploy/out .

deploy: build
	scp deploy/out/snek-server $(VM):/tmp/snek-server
	ssh $(VM) 'sudo install -m 755 /tmp/snek-server /opt/snekgame/snek-server && rm /tmp/snek-server && sudo systemctl restart snekgame && sleep 2 && systemctl is-active snekgame'

setup-vm:
	scp deploy/snekgame.service deploy/snek.caddyfile $(VM):/tmp/
	ssh $(VM) 'sudo sh -s' < deploy/setup-vm.sh
