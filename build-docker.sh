#!/usr/bin/env sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
IMAGE=usbipd-tui-builder:local

docker build --tag "$IMAGE" "$ROOT"
mkdir -p "$ROOT/target/docker"

container=$(docker create "$IMAGE")
cleanup() {
    docker rm -f "$container" >/dev/null
}
trap cleanup EXIT INT TERM

docker cp "$container:/build/target/release/usbipd-tui" "$ROOT/target/docker/usbipd-tui"
chmod +x "$ROOT/target/docker/usbipd-tui"
printf 'Built %s\n' "$ROOT/target/docker/usbipd-tui"
