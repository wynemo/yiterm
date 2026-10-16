#!/usr/bin/env bash
# Run only on a Linux runner, inside quay.io/pypa/manylinux2014_x86_64.
set -euo pipefail
cd /work
yum install -y libX11-devel libXcursor-devel libXi-devel libXrandr-devel \
  libxcb-devel libxkbcommon-devel libxkbcommon-x11 mesa-libGL-devel \
  mesa-dri-drivers mesa-demos xorg-x11-server-Xvfb xorg-x11-utils
export PATH="$CARGO_HOME/bin:/opt/python/cp313-cp313/bin:$PATH"
if ! command -v rustup >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/yiterm-rustup.sh
  sh /tmp/yiterm-rustup.sh -y --profile minimal --default-toolchain none
fi
rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal
rustup default "$RUST_TOOLCHAIN"
target=x86_64-unknown-linux-gnu
cargo build --locked --release --target "$target" --no-default-features --features x11
binary="target/$target/release/yiterm"
python3 scripts/verify_linux.py "$binary"
cargo test --locked --release --target "$target" --no-default-features --features x11 --test logging
cargo test --locked --release --target "$target" --no-default-features --features x11 --bin yiterm profiling::tests
cargo test --locked --release --target "$target" --no-default-features --features x11 --bin yiterm repaint_tests
cargo test --locked --release --target "$target" --no-default-features --features x11 --bin yiterm terminal_unicode_
# Exercise window/context creation with CentOS 7's Mesa software renderer.
Xvfb :99 -screen 0 1280x800x24 +extension GLX &
xvfb_pid=$!
trap 'kill "$xvfb_pid" 2>/dev/null || true' EXIT
export DISPLAY=:99 LIBGL_ALWAYS_SOFTWARE=1 WINIT_UNIX_BACKEND=x11
export LIBGL_DEBUG=verbose GALLIUM_DRIVER=llvmpipe
for attempt in {1..30}; do
  if xdpyinfo >/dev/null 2>&1; then break; fi
  sleep 1
done
xdpyinfo >/dev/null
glxinfo -B
python3 scripts/smoke_linux.py "$binary"
python3 scripts/package.py --target "$target"
