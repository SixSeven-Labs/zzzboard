#!/usr/bin/env bash
# Build zzzboard for linux/amd64 HERE, ship the image to the GCE VM, and run
# deploy.sh there with the pre-built image. Compiling Rust on a 1 GB e2-micro
# is slow and fragile; this keeps the VM to `docker load` + `compose up`.
#
#   ./ship.sh                      # defaults: VM zzzboard, zone us-east1-b
#   ZZZ_VM=x ZZZ_ZONE=y ./ship.sh
#
# Needs: rustup, cargo-zigbuild (+ zig), docker with buildx, gcloud (logged in,
# project set). Run from the repo root on a pushed commit: deploy.sh is fetched
# from GitHub main on the VM.
set -euo pipefail

VM=${ZZZ_VM:-zzzboard}
ZONE=${ZZZ_ZONE:-us-east1-b}
TARGET=x86_64-unknown-linux-musl
IMAGE=zzzboard:local
REMOTE_TAR=/tmp/zzzboard-image.tar.gz

cd "$(dirname "$0")"
if [[ -n $(git status --porcelain) ]]; then
  echo "ship.sh: working tree is dirty; commit and push first (the VM pulls deploy.sh from main)" >&2
  exit 1
fi

echo "==> cross-compiling for $TARGET"
command -v cargo-zigbuild >/dev/null || cargo install cargo-zigbuild
rustup target add "$TARGET" >/dev/null
cargo zigbuild --release --locked --target "$TARGET"

echo "==> building linux/amd64 image $IMAGE"
# A local (native-arch) zzzboard:local may exist for `docker compose up` on this
# machine; remember it so the tag can be handed back after the save.
native=$(docker image inspect "$IMAGE" --format '{{.Id}}' 2>/dev/null || true)
docker buildx build --platform linux/amd64 -f Dockerfile.ship -t "$IMAGE" --load .

tar=$(mktemp -t zzzboard-image.XXXXXX).tar.gz
trap 'rm -f "$tar"' EXIT
echo "==> saving image to $tar"
docker save "$IMAGE" | gzip >"$tar"
ls -la "$tar"
if [[ -n $native ]]; then
  docker tag "$IMAGE" zzzboard:shipped-amd64
  docker tag "$native" "$IMAGE"
fi

echo "==> copying to $VM ($ZONE)"
gcloud compute scp "$tar" "$VM:$REMOTE_TAR" --zone "$ZONE" --quiet

echo "==> running deploy.sh on $VM with the pre-built image"
gcloud compute ssh "$VM" --zone "$ZONE" --quiet --command \
  "sudo env ZZZ_IMAGE=$REMOTE_TAR bash -c 'curl -fsSL https://raw.githubusercontent.com/SixSeven-Labs/zzzboard/main/deploy.sh | bash'"
