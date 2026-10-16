#!/usr/bin/env python3
"""Compare synthetic terminal CPU and key-to-PTY latency under Linux/Xvfb."""
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile
import time


def measure(binary, label, output):
    with tempfile.TemporaryDirectory(prefix="yiterm-perf-") as directory:
        root = Path(directory)
        ack = root / "ack"
        active = root / "active"
        shell = root / "fixture"
        shell.write_text(f'''#!{sys.executable}
import os, pathlib, threading, time, tty
tty.setraw(0)
ack = pathlib.Path({str(ack)!r})
active = pathlib.Path({str(active)!r})
os.write(1, b"\\x1b[2J\\x1b[H" + (b"Terminal benchmark: abcdefghijklmnopqrstuvwxyz 0123456789 " * 2 + b"\\r\\n") * 32)
def refresh():
    counter = 0
    while True:
        if active.exists():
            counter += 1
            os.write(1, ("\\x1b[35;1HStatus update: %08d\\x1b[K" % counter).encode())
        time.sleep(0.2)
threading.Thread(target=refresh, daemon=True).start()
with ack.open("w", buffering=1) as stream:
    while True:
        key = os.read(0, 1)
        if not key: break
        stream.write(str(time.monotonic_ns()) + "\\n")
        os.write(1, key)
''')
        shell.chmod(0o755)
        env = dict(os.environ, XDG_CONFIG_HOME=directory, XDG_DATA_HOME=directory,
                   YITERM_PROFILE="1")
        process = subprocess.Popen([str(binary), "--shell", str(shell)], env=env)
        try:
            deadline = time.monotonic() + 30
            window = None
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError(f"{label} exited: {process.returncode}")
                result = subprocess.run(["xdotool", "search", "--onlyvisible", "--pid", str(process.pid)],
                                        capture_output=True, text=True)
                if result.returncode == 0 and result.stdout.strip():
                    window = result.stdout.splitlines()[0]
                    break
                time.sleep(0.2)
            if window is None:
                raise RuntimeError("No visible window")
            subprocess.run(["xdotool", "windowsize", window, "1877", "940"], check=True)
            subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True)
            time.sleep(3)

            def cpu_seconds():
                # /proc stat's comm can contain spaces; fields after ')' start at state.
                fields = Path(f"/proc/{process.pid}/stat").read_text().rsplit(")", 1)[1].split()
                return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")

            def sample_cpu():
                started, before = time.monotonic(), cpu_seconds()
                time.sleep(8)
                return 100 * (cpu_seconds() - before) / (time.monotonic() - started)

            idle_cpu = sample_cpu()
            active.touch()
            busy_cpu = sample_cpu()
            latencies = []
            for count in range(1, 41):
                started = time.monotonic_ns()
                subprocess.run(["xdotool", "key", "--clearmodifiers", "a"], check=True)
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline:
                    entries = ack.read_text().splitlines() if ack.exists() else []
                    if len(entries) >= count:
                        latencies.append((int(entries[count - 1]) - started) / 1e6)
                        break
                    time.sleep(0.002)
                else:
                    raise RuntimeError("Keystroke did not reach PTY within 3 seconds")
                time.sleep(0.08)
            result = dict(label=label, idle_cpu_percent=idle_cpu, updating_cpu_percent=busy_cpu,
                          key_to_pty_median_ms=statistics.median(latencies),
                          key_to_pty_p95_ms=sorted(latencies)[int(len(latencies) * 0.95) - 1])
            print(json.dumps(result), flush=True)
            return result
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            logs = "\n".join(p.read_text(errors="replace") for p in root.rglob("*.log"))
            (output / f"{label}.log").write_text(logs)


if __name__ == "__main__":
    output = Path("performance-results")
    output.mkdir(exist_ok=True)
    results = [measure(Path(binary).resolve(), label, output)
               for label, binary in zip(("baseline", "candidate"), sys.argv[1:], strict=True)]
    (output / "comparison.json").write_text(json.dumps(results, indent=2))
