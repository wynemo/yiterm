#!/usr/bin/env python3
"""Package a release binary, checking its architecture before upload."""
import argparse
import hashlib
import io
import os
import plistlib
import shutil
from pathlib import Path
import struct
import tarfile
import json
import subprocess
import tempfile
import zipfile
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parents[1]
TARGETS = {
    "x86_64-pc-windows-msvc": ("windows-x64", "yiterm.exe"),
    "aarch64-apple-darwin": ("macos-arm64", "yiterm"),
    "x86_64-unknown-linux-gnu": ("linux-x64", "yiterm"),
}
# 1.25 includes cursor resynchronization after unknown VT/APC passthrough.
CONPTY_VERSION = "1.25.260930003"
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


def verify_binary(data: bytes, target: str, subsystem=None) -> None:
    if target == "x86_64-pc-windows-msvc":
        if len(data) < 64 or data[:2] != b"MZ":
            raise ValueError("Expected a Windows PE executable")
        offset = struct.unpack_from("<I", data, 0x3C)[0]
        if offset + 6 > len(data) or data[offset:offset + 4] != b"PE\0\0":
            raise ValueError("Invalid Windows PE header")
        if struct.unpack_from("<H", data, offset + 4)[0] != 0x8664:
            raise ValueError("Expected a Windows x64 executable")
        if subsystem is not None:
            if offset + 94 > len(data) or struct.unpack_from("<H", data, offset + 92)[0] != subsystem:
                raise ValueError(f"Expected Windows subsystem {subsystem}")
    elif target == "x86_64-unknown-linux-gnu":
        if len(data) < 64 or data[:7] != b"\x7fELF\x02\x01\x01":
            raise ValueError("Expected a little-endian ELF64 executable")
        if struct.unpack_from("<H", data, 18)[0] != 62:
            raise ValueError("Expected a Linux x64 executable")
        if struct.unpack_from("<H", data, 16)[0] not in (2, 3):
            raise ValueError("Expected an ELF executable or PIE")
    else:
        if len(data) < 8 or data[:4] != b"\xcf\xfa\xed\xfe":
            raise ValueError("Expected a 64-bit Mach-O executable")
        if struct.unpack_from("<I", data, 4)[0] != 0x0100000C:
            raise ValueError("Expected a macOS arm64 executable")


def prepare_linux_binary(binary: Path) -> Path:
    """Split symbols on a copy, keeping Cargo's cached build intact."""
    directory = binary.parent / "profiling"
    directory.mkdir(exist_ok=True)
    packaged = directory / binary.name
    symbols = directory / f"{binary.name}.debug"
    subprocess.run(["objcopy", "--only-keep-debug", str(binary), str(symbols)], check=True)
    shutil.copy2(binary, packaged)
    subprocess.run(["strip", "--strip-unneeded", str(packaged)], check=True)
    subprocess.run(["objcopy", f"--add-gnu-debuglink={symbols}", str(packaged)], check=True)
    sections = subprocess.check_output(["readelf", "--sections", "--wide", str(packaged)], text=True)
    debug_sections = subprocess.check_output(["readelf", "--sections", "--wide", str(symbols)], text=True)
    if ".debug_info" in sections or ".debug_info" not in debug_sections:
        raise ValueError("Linux debug symbol separation failed")
    verify_binary(packaged.read_bytes(), "x86_64-unknown-linux-gnu")
    print(f"Linux executable: {binary.stat().st_size / 2**20:.1f} MiB -> "
          f"{packaged.stat().st_size / 2**20:.1f} MiB; symbols stored separately")
    return packaged


def prepare_macos_bundle(binary: Path, version: str, directory: Path) -> Path:
    """Build a Finder/Dock application while retaining the standalone CLI."""
    app = directory / "YiTerm.app"
    contents = app / "Contents"
    (contents / "MacOS").mkdir(parents=True)
    (contents / "Resources").mkdir()
    shutil.copy2(binary, contents / "MacOS/yiterm")
    (contents / "MacOS/yiterm").chmod(0o755)
    shutil.copy2(ROOT / "assets/icons/yiterm.icns", contents / "Resources/yiterm.icns")
    with (contents / "Info.plist").open("wb") as output:
        plistlib.dump({
            "CFBundleName": "YiTerm",
            "CFBundleDisplayName": "YiTerm",
            "CFBundleIdentifier": "io.github.wynemo.yiterm",
            "CFBundleExecutable": "yiterm",
            "CFBundleIconFile": "yiterm.icns",
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": version,
            "CFBundleVersion": version,
            "NSHighResolutionCapable": True,
            "LSMinimumSystemVersion": "11.0",
        }, output)
    # Ad-hoc signing covers both the executable and bundle resources.
    subprocess.run(["codesign", "--force", "--deep", "--sign", "-", str(app)], check=True)
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True)
    return app


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
    if args.target == "x86_64-unknown-linux-gnu":
        binary = prepare_linux_binary(binary)
    name = f"yiterm-{version}-{platform}"
    files = [
        (binary, f"{name}/{executable}"),
        (ROOT / "readme.md", f"{name}/readme.md"),
        (ROOT / "readme.zhcn.md", f"{name}/readme.zhcn.md"),
        (ROOT / "TECHNICAL.md", f"{name}/TECHNICAL.md"),
        (ROOT / "THIRD_PARTY_NOTICES.md", f"{name}/THIRD_PARTY_NOTICES.md"),
        (ROOT / "vendor/alacritty_terminal/LICENSE-APACHE", f"{name}/licenses/alacritty-LICENSE-APACHE"),
        (ROOT / "assets/fonts/ibm-plex-sans/OFL.txt", f"{name}/licenses/IBMPlexSans-OFL.txt"),
        (ROOT / "assets/fonts/lilex/OFL.txt", f"{name}/licenses/Lilex-OFL.txt"),
    ]
    if args.target == "x86_64-unknown-linux-gnu":
        files.extend([
            (ROOT / "scripts/install_linux_desktop.sh", f"{name}/install-desktop.sh"),
            (ROOT / "assets/icons/yiterm.png", f"{name}/yiterm.png"),
            (ROOT / "vendor/glutin/LICENSE", f"{name}/licenses/glutin/LICENSE"),
            (ROOT / "vendor/eframe/LICENSE-MIT", f"{name}/licenses/eframe/LICENSE-MIT"),
            (ROOT / "vendor/eframe/LICENSE-APACHE", f"{name}/licenses/eframe/LICENSE-APACHE"),
            (ROOT / "vendor/egui-winit/LICENSE-MIT", f"{name}/licenses/egui-winit/LICENSE-MIT"),
            (ROOT / "vendor/egui-winit/LICENSE-APACHE", f"{name}/licenses/egui-winit/LICENSE-APACHE"),
        ])
    else:
        files.append((ROOT / "licenses/gpui-LICENSE-APACHE", f"{name}/licenses/gpui-LICENSE-APACHE"))
    output = ROOT / "dist"
    output.mkdir(exist_ok=True)
    if args.target.endswith("windows-msvc"):
        # portable-pty loads an app-local conpty.dll and its OpenConsole.exe.
        # The system ConPTY can swallow native console mouse-mode changes;
        # ship the same runtime family used by Windows Terminal instead.
        for helper_name in ("ssh.exe", "mosh.exe"):
            helper = binary.parent / helper_name
            verify_binary(helper.read_bytes(), args.target, subsystem=3)
            files.append((helper, f"{name}/bin/{helper_name}"))
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
        with tempfile.TemporaryDirectory(prefix="yiterm-package-") as temporary:
            if args.target == "aarch64-apple-darwin":
                app = prepare_macos_bundle(binary, version, Path(temporary))
                files.extend((source, f"{name}/YiTerm.app/{source.relative_to(app)}")
                             for source in sorted(app.rglob("*")) if source.is_file())
            with tarfile.open(archive, "w:gz") as package:
                for source, destination in files:
                    info = package.gettarinfo(str(source), destination)
                    info.mode = 0o755 if (source == binary or destination.endswith(
                        ("/Contents/MacOS/yiterm", "/install-desktop.sh")
                    )) else 0o644
                    with source.open("rb") as content:
                        package.addfile(info, content)
    checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_suffix(archive.suffix + ".sha256").write_text(
        f"{checksum}  {archive.name}\n", encoding="utf-8"
    )
    if github_output := os.environ.get("GITHUB_OUTPUT"):
        with open(github_output, "a", encoding="utf-8") as outputs:
            outputs.write(f"version={version}\n")
            tags = subprocess.check_output(
                ["git", "tag", "--points-at", "HEAD"], cwd=ROOT, text=True,
            ).splitlines()
            if f"v{version}" in tags:
                outputs.write(f"tag=v{version}\n")
    print(f"Packaged {archive.name}")


if __name__ == "__main__":
    main()
