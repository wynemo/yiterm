#!/usr/bin/env bash
# Run only on a Linux runner, inside quay.io/pypa/manylinux2014_x86_64.
set -euo pipefail
cd /work
stage=${1:?Expected build or test}
case "$stage" in build|test) ;; *) exit 2 ;; esac
yum install -y libX11-devel libXcursor-devel libXi-devel libXrandr-devel \
  libxcb-devel libxkbcommon-devel libxkbcommon-x11 mesa-libGL-devel \
  mesa-dri-drivers
if [[ "$stage" == test ]]; then
  yum install -y glx-utils xorg-x11-server-Xvfb xorg-x11-utils fontconfig google-noto-sans-cjk-fonts
fi
export PATH="$CARGO_HOME/bin:/opt/python/cp313-cp313/bin:$PATH"
if ! command -v rustup >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/yiterm-rustup.sh
  sh /tmp/yiterm-rustup.sh -y --profile minimal --default-toolchain none
fi
rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal
rustup default "$RUST_TOOLCHAIN"
target=x86_64-unknown-linux-gnu
if [[ "$stage" == build ]]; then
  # The checkout belongs to the host runner, not the container root user.
  git config --global --add safe.directory /work
  cargo build --locked --release --target "$target" --no-default-features --features x11
  python3 scripts/verify_linux.py "target/$target/release/yiterm"
  python3 scripts/package.py --target "$target"
  exit 0
fi
# Test the uploaded package, rather than a separately rebuilt GUI executable.
binary=$(find /work/dist -type f -name yiterm -print -quit)
: "${binary:?Missing extracted Linux package}"
python3 scripts/verify_linux.py "$binary"
cargo test --locked --release --target "$target" --no-default-features --features x11 --test logging
cargo test --locked --release --target "$target" --no-default-features --features x11 --lib update::tests
cargo test --locked --release --target "$target" --no-default-features --features x11 --test update
cargo test --locked --release --target "$target" --no-default-features --features x11 --bin yiterm about_dialog_keeps_content_inside_a_compact_window
cargo test --locked --release --target "$target" --no-default-features --features x11 --bin yiterm profiling::tests
cargo test --locked --release --target "$target" --no-default-features --features x11 --bin yiterm repaint_tests
cargo test --locked --release --target "$target" --no-default-features --features x11 --bin yiterm terminal_unicode_
cargo test --locked --release --target "$target" --no-default-features --features x11 --bin yiterm settings_font_picker_
cargo test --locked --release --target "$target" --no-default-features --features x11 --bin yiterm terminal_unicode_linux_system_cjk_fallback -- --ignored
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
glxinfo
python3 scripts/smoke_linux.py "$binary"
