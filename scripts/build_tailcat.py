#!/usr/bin/env python3
"""Build the pinned native sharing helper for the selected release target."""
import os
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parents[1]
target = sys.argv[1] if len(sys.argv) > 1 else "aarch64-apple-darwin"
platforms = {"aarch64-apple-darwin": ("darwin", "arm64"),
             "x86_64-pc-windows-msvc": ("windows", "amd64"),
             "x86_64-unknown-linux-gnu": ("linux", "amd64")}
goos, arch = platforms[target]
# Windows builds belong on the Windows CI runner.
if goos == "windows" and sys.platform != "win32":
    raise SystemExit("Build the Windows helper on the Windows CI runner")
directory = root / "target" / target / "release" if len(sys.argv) > 1 else root / "target/release"
output = directory / ("yiterm-tailcat.exe" if goos == "windows" else "yiterm-tailcat")
output.parent.mkdir(parents=True, exist_ok=True)
env = dict(os.environ, GOTOOLCHAIN="go1.27.4", GOOS=goos, GOARCH=arch, CGO_ENABLED="0")
subprocess.run(["go", "build", "-trimpath", "-ldflags=-s -w", "-o", str(output), "."],
               cwd=root / "tools/tailcat", env=env, check=True)
