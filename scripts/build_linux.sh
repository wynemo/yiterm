#!/usr/bin/env bash
# Run only on a Linux runner, inside quay.io/pypa/manylinux2014_x86_64.
set -euo pipefail
cd /work
case "${1:-build}" in build) ;; *) exit 2 ;; esac
yum install -y libX11-devel libXcursor-devel libXi-devel libXrandr-devel \
  libxcb-devel libxkbcommon-devel libxkbcommon-x11 mesa-libGL-devel \
  mesa-dri-drivers
export PATH="$CARGO_HOME/bin:/opt/python/cp313-cp313/bin:$PATH"
if ! command -v rustup >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/yiterm-rustup.sh
  sh /tmp/yiterm-rustup.sh -y --profile minimal --default-toolchain none
fi
rustup toolchain install "$RUST_TOOLCHAIN" --profile minimal
rustup default "$RUST_TOOLCHAIN"
target=x86_64-unknown-linux-gnu
# The checkout belongs to the host runner, not the container root user.
git config --global --add safe.directory /work
cargo build --locked --release --target "$target" --no-default-features --features x11
python3 scripts/verify_linux.py "target/$target/release/yiterm"
python3 scripts/package.py --target "$target"
