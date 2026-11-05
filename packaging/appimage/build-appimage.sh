#!/usr/bin/env bash
# Build Lucentia's AppImage inside an Ubuntu 24.04 container (Docker or Podman), so
# the result runs on most current distros (glibc 2.39+: Arch, SteamOS, Fedora 40+,
# Ubuntu 24.04+, Debian 13). Output: dist/Lucentia-x86_64.AppImage
#
# What's inside: Lucentia, GStreamer and the plugins it uses. What's not: the GPU
# drivers (Mesa, VA-API, libdrm), PipeWire's client library and Wayland/X11, which
# always come from the system so hardware encoding matches the user's drivers.
#
# Extra container arguments (e.g. a proxy) go in LUCENTIA_CONTAINER_ARGS.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
engine=${CONTAINER_ENGINE:-$(command -v podman || command -v docker || true)}
if [ -z "$engine" ]; then
    printf '\033[31merror:\033[0m needs Docker or Podman\n' >&2
    exit 1
fi

# shellcheck disable=SC2086
"$engine" run --rm \
    -v "$repo":/src:z \
    -v lucentia-appimage-cache:/cache \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    -e NO_COLOR="${NO_COLOR:-}" \
    ${LUCENTIA_CONTAINER_ARGS:-} \
    ubuntu:24.04 bash /src/packaging/appimage/inside.sh
