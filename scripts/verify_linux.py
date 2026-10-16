#!/usr/bin/env python3
"""Reject Linux binaries requiring GLIBC newer than CentOS 7's 2.17."""
import re
import subprocess
import sys
from pathlib import Path
from package import verify_binary


def verify_glibc(output: str) -> None:
    versions = re.findall(r"\bGLIBC_(\d+(?:\.\d+)+)\b", output)
    if not versions:
        raise ValueError("No GLIBC version requirements found")
    unsupported = sorted({v for v in versions if tuple(map(int, v.split('.'))) > (2, 17)})
    if unsupported or "GLIBC_PRIVATE" in output:
        raise ValueError(f"Unsupported GLIBC requirements: {unsupported or ['GLIBC_PRIVATE']}")


if __name__ == "__main__":
    binary = Path(sys.argv[1])
    verify_binary(binary.read_bytes(), "x86_64-unknown-linux-gnu")
    output = subprocess.check_output(["readelf", "--version-info", str(binary)], text=True)
    verify_glibc(output)
    # Resolve direct shared libraries in the CentOS 7 environment too.
    libraries = subprocess.check_output(["ldd", str(binary)], text=True)
    print(libraries)
    if "not found" in libraries:
        raise ValueError("Missing shared libraries")
    print("Linux x64 ELF and GLIBC <= 2.17 verified")
