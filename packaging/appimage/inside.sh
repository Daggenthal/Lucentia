#!/usr/bin/env bash
# Runs inside the Ubuntu 24.04 build container. See build-appimage.sh.
set -euo pipefail

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    step() { printf '\033[36m==>\033[0m %s\n' "$*"; }
    warn() { printf '\033[33mwarning:\033[0m %s\n' "$*"; }
    ok() { printf '\033[32mok:\033[0m %s\n' "$*"; }
else
    step() { printf '==> %s\n' "$*"; }
    warn() { printf 'warning: %s\n' "$*"; }
    ok() { printf 'ok: %s\n' "$*"; }
fi

# Optional: apt over HTTPS with an extra CA (for build machines behind a TLS proxy).
if [ -n "${APT_CA:-}" ]; then
    sed -i 's|http://|https://|g' /etc/apt/sources.list.d/ubuntu.sources
    echo "Acquire::https::CaInfo \"$APT_CA\";" > /etc/apt/apt.conf.d/99ca
fi

step "installing build tools and GStreamer"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
    build-essential pkg-config curl ca-certificates file patchelf \
    libglib2.0-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    libfontconfig-dev libfreetype-dev libxkbcommon-dev libwayland-dev \
    libx11-dev libxcursor-dev libxrandr-dev libxi-dev libgl-dev libegl-dev \
    gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-gl \
    gstreamer1.0-pulseaudio gstreamer1.0-pipewire >/dev/null

export CARGO_HOME=/cache/cargo RUSTUP_HOME=/cache/rustup PATH=/cache/cargo/bin:$PATH
if ! command -v cargo >/dev/null; then
    step "installing Rust"
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path >/dev/null
fi

step "building Lucentia (release)"
cd /src
CARGO_TARGET_DIR=/cache/target cargo build --release --locked
bin=/cache/target/release/lucentia

tools=/cache/tools
mkdir -p "$tools"
fetch() {
    [ -s "$tools/$1" ] || curl -sSfL -o "$tools/$1" "$2"
    chmod +x "$tools/$1"
}
step "fetching linuxdeploy"
fetch linuxdeploy-x86_64.AppImage \
    https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
fetch appimagetool-x86_64.AppImage \
    https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
# The AppImage runtime, fetched here rather than by appimagetool (which can't go
# through a proxy).
fetch runtime-x86_64 \
    https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64
export APPIMAGE_EXTRACT_AND_RUN=1 PATH="$tools:$PATH"

step "assembling the AppDir"
appdir=/tmp/AppDir
rm -rf "$appdir"
gst_sys=/usr/lib/x86_64-linux-gnu/gstreamer-1.0
plugins="$appdir/usr/lib/gstreamer-1.0"
mkdir -p "$plugins" "$appdir/usr/lib/gstreamer1.0/gstreamer-1.0" "$appdir/apprun-hooks"

# Only the plugins Lucentia uses, to keep the AppImage small.
for p in coreelements app videoconvertscale videorate videotestsrc audiotestsrc \
         audioconvert audioresample audiomixer volume level autodetect \
         pulseaudio pipewire video4linux2 matroska isomp4 audioparsers \
         videoparsersbad libav x264 x265 svtav1 opus va opengl jpeg png \
         playback typefindfunctions debug imagefreeze audiobuffersplit; do
    f="$gst_sys/libgst$p.so"
    if [ -f "$f" ]; then
        cp "$f" "$plugins/"
    else
        warn "plugin $p not available in this GStreamer build, skipping"
    fi
done
cp /usr/lib/x86_64-linux-gnu/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner \
    "$appdir/usr/lib/gstreamer1.0/gstreamer-1.0/"

cat > "$appdir/apprun-hooks/lucentia-gstreamer.sh" <<'HOOK'
# Use the bundled GStreamer plugins only, with their own registry cache.
export GST_PLUGIN_SYSTEM_PATH_1_0="${APPDIR}/usr/lib/gstreamer-1.0"
export GST_PLUGIN_PATH_1_0=""
export GST_PLUGIN_SCANNER_1_0="${APPDIR}/usr/lib/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner"
export GST_REGISTRY_REUSE_PLUGIN_SCANNER="no"
export GST_REGISTRY_1_0="${XDG_CACHE_HOME:-$HOME/.cache}/lucentia/gst-registry-appimage.bin"
mkdir -p "$(dirname "$GST_REGISTRY_1_0")"
HOOK

step "deploying libraries"
cd /tmp
out=Lucentia-x86_64.AppImage
rm -f "$out"
# GPU drivers and display/PipeWire libraries come from the system, never the bundle:
# they have to match the user's kernel, Mesa and running PipeWire.
linuxdeploy-x86_64.AppImage --appdir "$appdir" \
    --executable "$bin" \
    --deploy-deps-only "$plugins" \
    --deploy-deps-only "$appdir/usr/lib/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner" \
    --desktop-file /src/packaging/appimage/lucentia.desktop \
    --icon-file /src/assets/lucentia.svg \
    --exclude-library 'libva.so*' --exclude-library 'libva-*.so*' \
    --exclude-library 'libdrm*.so*' --exclude-library 'libgbm.so*' \
    --exclude-library 'libEGL*.so*' --exclude-library 'libGL*.so*' \
    --exclude-library 'libvulkan.so*' --exclude-library 'libwayland-*.so*' \
    --exclude-library 'libpipewire-0.3.so*' \
    >/tmp/linuxdeploy.log 2>&1 || { tail -40 /tmp/linuxdeploy.log; exit 1; }

step "packing the AppImage"
ARCH=x86_64 appimagetool-x86_64.AppImage --runtime-file "$tools/runtime-x86_64" \
    "$appdir" "$out" >/tmp/appimagetool.log 2>&1 || { tail -30 /tmp/appimagetool.log; exit 1; }

mkdir -p /src/dist
mv "/tmp/$out" /src/dist/
chown -R "${HOST_UID:-0}:${HOST_GID:-0}" /src/dist
ok "dist/$out ($(du -h "/src/dist/$out" | cut -f1))"
