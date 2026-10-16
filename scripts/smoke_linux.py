#!/usr/bin/env python3
"""Require a mapped YiTerm window and a live process under Xvfb."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

with tempfile.TemporaryDirectory(prefix="yiterm-smoke-") as directory:
    env = dict(os.environ, XDG_CONFIG_HOME=directory, XDG_DATA_HOME=directory)
    process = subprocess.Popen([sys.argv[1], "--shell", "/bin/bash"], env=env)
    try:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(f"YiTerm exited during startup: {process.returncode}")
            tree = subprocess.check_output(["xwininfo", "-root", "-tree"], text=True)
            if '"YiTerm' in tree or '"yiterm' in tree:
                time.sleep(3)
                if process.poll() is not None:
                    raise RuntimeError("YiTerm exited after window creation")
                print("YiTerm X11 window startup passed")
                break
            time.sleep(0.5)
        else:
            raise RuntimeError("YiTerm did not create an X11 window")
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        for log in Path(directory).rglob("*.log"):
            print(log.read_text(errors="replace"))
