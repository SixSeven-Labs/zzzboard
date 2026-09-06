#!/usr/bin/env bash
# zzzboard deploy. Run ON the Ubuntu 24.04 VM as root. Idempotent: re-run for
# every update. Never modifies existing board data in /var/lib/zzzboard.
#
#   first time:  curl -fsSL https://raw.githubusercontent.com/SixSeven-Labs/zzzboard/main/deploy.sh | bash
#   afterwards:  /opt/zzzboard/deploy.sh
#
# What it does: installs Docker (Ubuntu packages) if missing; adds swap; clones
# or fast-forwards this repo into /opt/zzzboard; creates /var/lib/zzzboard owned
# by the container's non-root uid only if it does not exist; writes .env; loads
# a pre-built image if ZZZ_IMAGE points at one (see ship.sh) else builds; starts
# the compose stack; installs a systemd unit so it returns on reboot.
set -euo pipefail

REPO="${ZZZ_REPO:-https://github.com/SixSeven-Labs/zzzboard.git}"
BRANCH="${ZZZ_BRANCH:-main}"
APP_DIR=/opt/zzzboard
DATA_DIR=/var/lib/zzzboard
APP_UID=65532 # distroless "nonroot"

[[ $EUID -eq 0 ]] || { echo "deploy.sh: run as root" >&2; exit 1; }
log() { printf '\n==> %s\n' "$*"; }

log "packages"
export DEBIAN_FRONTEND=noninteractive
if ! command -v docker >/dev/null 2>&1 || ! docker compose version >/dev/null 2>&1; then
  apt-get update -q
  apt-get install -y -q docker.io docker-compose-v2 docker-buildx
fi
command -v git >/dev/null 2>&1 || apt-get install -y -q git
systemctl enable --now docker

log "swap (the Rust build needs more than an e2-micro's 1 GB)"
if [[ -z $(swapon --show --noheadings) ]]; then
  fallocate -l 2G /swapfile && chmod 600 /swapfile && mkswap -q /swapfile && swapon /swapfile
  grep -q '^/swapfile' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
  echo "2 GB swapfile created"
else
  echo "already present: $(swapon --show --noheadings | awk '{print $1, $3}')"
fi

log "data dir $DATA_DIR"
if [[ -d $DATA_DIR ]]; then
  echo "exists, left untouched ($(du -sh "$DATA_DIR" | cut -f1))"
else
  install -d -m 0750 -o "$APP_UID" -g "$APP_UID" "$DATA_DIR"
  echo "created, owned by uid $APP_UID"
fi

log "code -> $APP_DIR ($BRANCH)"
if [[ -d $APP_DIR/.git ]]; then
  git -C "$APP_DIR" fetch --quiet origin "$BRANCH"
  git -C "$APP_DIR" reset --quiet --hard "origin/$BRANCH"
else
  git clone --quiet --branch "$BRANCH" "$REPO" "$APP_DIR"
fi
cd "$APP_DIR"
git log -1 --format='at %h %s (%ci)'

log "config"
cat > .env <<EOF
# written by deploy.sh; edit deploy.sh, not this file
ZZZBOARD_DATA=$DATA_DIR
ZZZ_TLS=abuse@zzzboard.org
ZZZ_BASE_URL=https://zzzboard.org
EOF
chmod 0600 .env
install -d -m 0750 caddy_data caddy_config

log "image"
if [[ -n ${ZZZ_IMAGE:-} && -f ${ZZZ_IMAGE:-} ]]; then
  echo "loading pre-built image from $ZZZ_IMAGE (shipped by ship.sh)"
  loaded=$(docker load -i "$ZZZ_IMAGE" | tee /dev/stderr | sed -n 's/^Loaded image: //p' | head -1)
  [[ -n $loaded ]] || { echo "deploy.sh: docker load reported no image" >&2; exit 1; }
  docker tag "$loaded" zzzboard:local
  rm -f "$ZZZ_IMAGE"
else
  echo "building here (slow on an e2-micro; prefer ship.sh from a workstation)"
  docker compose build --pull --quiet
fi

log "start"
docker compose up -d --remove-orphans

log "caddy reload (Caddyfile is a bind mount; compose does not restart caddy when it changes)"
# Right after `up`, Caddy's admin socket can take a second to appear; retry
# before falling back to a restart, which would bounce port 443.
for attempt in 1 2 3 4 5 6; do
  if docker compose exec -T caddy caddy reload --config /etc/caddy/Caddyfile --adapter caddyfile >/dev/null 2>&1; then
    echo "reloaded (attempt $attempt)"
    break
  fi
  if [[ $attempt == 6 ]]; then
    echo "reload failed after $attempt attempts; restarting caddy"
    docker compose restart caddy
  else
    sleep 2
  fi
done

log "systemd"
cat > /etc/systemd/system/zzzboard.service <<EOF
[Unit]
Description=zzzboard (docker compose stack)
Requires=docker.service
After=docker.service network-online.target
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
WorkingDirectory=$APP_DIR
ExecStart=/usr/bin/docker compose up -d --remove-orphans
ExecStop=/usr/bin/docker compose stop
TimeoutStartSec=300

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable --quiet zzzboard.service
systemctl start zzzboard.service

log "status"
docker compose ps
echo
echo "zzzboard is up. data in $DATA_DIR. check: curl -s http://127.0.0.1:8080/ | head -5"
