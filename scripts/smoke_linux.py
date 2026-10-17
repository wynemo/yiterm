#!/usr/bin/env python3
"""Require a mapped YiTerm window and a live process under Xvfb."""
import os
import re
from pathlib import Path
import subprocess
import sys
import tempfile
import time

with tempfile.TemporaryDirectory(prefix="yiterm-smoke-") as directory:
    env = dict(os.environ, XDG_CONFIG_HOME=directory, XDG_DATA_HOME=directory, YITERM_PROFILE="1")
    process = subprocess.Popen([sys.argv[1], "--shell", "/bin/bash"], env=env)
    try:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(f"YiTerm exited during startup: {process.returncode}")
            tree = subprocess.check_output(["xwininfo", "-root", "-tree"], text=True)
            window = re.search(r'^\s*(0x[0-9a-f]+)\s+"(?:YiTerm|yiterm)', tree, re.MULTILINE)
            mapped = False
            if window:
                details = subprocess.check_output(["xwininfo", "-id", window[1]], text=True)
                mapped = "Map State: IsViewable" in details
            if mapped:
                time.sleep(3)
                if process.poll() is not None:
                    raise RuntimeError("YiTerm exited after window creation")
                logs = "\n".join(p.read_text(errors="replace") for p in Path(directory).rglob("*.log"))
                if "X11 error:" in logs or "GLXBadContextTag" in logs:
                    raise RuntimeError("X11/GLX error during window startup")
                if "GPUI window initialized" not in logs:
                    raise RuntimeError("No initialized GPUI window was logged")
                print("YiTerm GPUI X11 window startup passed")
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
