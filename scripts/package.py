#!/usr/bin/env python3
"""Package a release binary, checking its architecture before upload."""
import argparse
import hashlib
import io
import os
from pathlib import Path
import struct
import tarfile
import json
import subprocess
import zipfile
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parents[1]
TARGETS = {
    "x86_64-pc-windows-msvc": ("windows-x64", "yiterm.exe"),
    "aarch64-apple-darwin": ("macos-arm64", "yiterm"),
}
CONPTY_VERSION = "1.24.261001001"
CONPTY_FILES = {
    "runtimes/win-x64/native/conpty.dll": "conpty.dll",
    "build/native/runtimes/x64/OpenConsole.exe": "OpenConsole.exe",
}


def conpty_files(data: bytes) -> dict[str, bytes]:
    """Require the matched x64 DLL and console host from Microsoft's package."""
    with zipfile.ZipFile(io.BytesIO(data)) as package:
        files = {name: package.read(source) for source, name in CONPTY_FILES.items()}
    for content in files.values():
        verify_binary(content, "x86_64-pc-windows-msvc")
    return files


def download_conpty() -> dict[str, bytes]:
    url = (
        "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/"
        f"{CONPTY_VERSION}/microsoft.windows.console.conpty.{CONPTY_VERSION}.nupkg"
    )
    with urlopen(url, timeout=60) as response:
        return conpty_files(response.read())


def verify_binary(data: bytes, target: str) -> None:
    if target == "x86_64-pc-windows-msvc":
        if len(data) < 64 or data[:2] != b"MZ":
            raise ValueError("Expected a Windows PE executable")
        offset = struct.unpack_from("<I", data, 0x3C)[0]
        if offset + 6 > len(data) or data[offset:offset + 4] != b"PE\0\0":
            raise ValueError("Invalid Windows PE header")
        if struct.unpack_from("<H", data, offset + 4)[0] != 0x8664:
            raise ValueError("Expected a Windows x64 executable")
    else:
        if len(data) < 8 or data[:4] != b"\xcf\xfa\xed\xfe":
            raise ValueError("Expected a 64-bit Mach-O executable")
        if struct.unpack_from("<I", data, 4)[0] != 0x0100000C:
            raise ValueError("Expected a macOS arm64 executable")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=TARGETS)
    args = parser.parse_args()
    platform, executable = TARGETS[args.target]
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"],
        cwd=ROOT, text=True,
    ))
    version = next(package["version"] for package in metadata["packages"]
                   if package["name"] == "yiterm" and package["id"] in metadata["workspace_members"])
    binary = ROOT / "target" / args.target / "release" / executable
    verify_binary(binary.read_bytes(), args.target)
    name = f"yiterm-{version}-{platform}"
    files = [
        (binary, f"{name}/{executable}"),
        (ROOT / "readme.md", f"{name}/readme.md"),
        (ROOT / "TECHNICAL.md", f"{name}/TECHNICAL.md"),
        (ROOT / "vendor/egui-winit/LICENSE-MIT", f"{name}/licenses/egui-winit/LICENSE-MIT"),
        (ROOT / "vendor/egui-winit/LICENSE-APACHE", f"{name}/licenses/egui-winit/LICENSE-APACHE"),
    ]
    output = ROOT / "dist"
    output.mkdir(exist_ok=True)
    if args.target.endswith("windows-msvc"):
        # portable-pty loads an app-local conpty.dll and its OpenConsole.exe.
        # The system ConPTY can swallow native console mouse-mode changes;
        # ship the same runtime family used by Windows Terminal instead.
        runtime = download_conpty()
        files.append((ROOT / "licenses/conpty-LICENSE", f"{name}/licenses/conpty-LICENSE"))
        archive = output / f"{name}.zip"
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as package:
            for source, destination in files:
                package.write(source, destination)
            for filename, content in runtime.items():
                package.writestr(f"{name}/{filename}", content)
    else:
        archive = output / f"{name}.tar.gz"
        with tarfile.open(archive, "w:gz") as package:
            for source, destination in files:
                info = package.gettarinfo(str(source), destination)
                info.mode = 0o755 if source == binary else 0o644
                with source.open("rb") as content:
                    package.addfile(info, content)
    checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_suffix(archive.suffix + ".sha256").write_text(
        f"{checksum}  {archive.name}\n", encoding="utf-8"
    )
    if github_output := os.environ.get("GITHUB_OUTPUT"):
        with open(github_output, "a", encoding="utf-8") as outputs:
            outputs.write(f"version={version}\n")
    print(f"Packaged {archive.name}")


if __name__ == "__main__":
    main()
