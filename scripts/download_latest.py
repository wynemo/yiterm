#!/usr/bin/env python3
"""Download the latest successful Windows build into the current directory.

Requires GitHub CLI (gh). Run gh auth login before using this script.
"""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import tempfile
from urllib.parse import urlencode
import zipfile

FILES = ("yiterm.exe", "bin/ssh.exe", "conpty.dll", "OpenConsole.exe")


class GitHub:
    def __init__(self, repo):
        self.base = f"repos/{repo}"

    def get(self, path):
        result = subprocess.run(
            ["gh", "api", "--hostname", "github.com", "--method", "GET",
             self.base + path],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=180,
        )
        if result.returncode:
            detail = result.stderr.decode("utf-8", errors="replace").strip()
            raise ValueError(f"gh 请求失败：{detail}\n请检查 gh auth login 登录状态及仓库 Actions 读取权限。")
        return result.stdout

    def json(self, path):
        return json.loads(self.get(path))

    def pages(self, path, key, params=None):
        page = 1
        while True:
            query = urlencode(dict(params or {}, per_page=100, page=page))
            items = self.json(f"{path}?{query}")[key]
            yield from items
            if len(items) < 100:
                return
            page += 1


def latest_artifact(api, branch):
    runs = api.pages("/actions/workflows/ci.yml/runs", "workflow_runs", {
        "branch": branch, "status": "success", "event": "push",
    })
    for run in runs:
        artifacts = api.pages(f"/actions/runs/{run['id']}/artifacts", "artifacts")
        candidates = [a for a in artifacts if not a["expired"]
                      and a["name"].startswith("yiterm-")
                      and a["name"].endswith("-windows-x64")]
        if candidates:
            return run, max(candidates, key=lambda a: a["id"])
    raise ValueError(f"分支 {branch} 没有成功且未过期的 Windows x64 构建产物")


def unpack(data):
    """Read the nested release ZIP without extracting arbitrary archive paths."""
    with zipfile.ZipFile(io.BytesIO(data)) as artifact:
        archives = [n for n in artifact.namelist() if n.endswith("-windows-x64.zip")]
        if len(archives) != 1:
            raise ValueError("Actions 产物中未找到唯一的 Windows 压缩包")
        archive = archives[0]
        package_data = artifact.read(archive)
        expected = artifact.read(archive + ".sha256").decode().split()[0]
        if hashlib.sha256(package_data).hexdigest() != expected:
            raise ValueError("Windows 压缩包 SHA-256 校验失败")
    with zipfile.ZipFile(io.BytesIO(package_data)) as package:
        result = {}
        for filename in FILES:
            matches = [n for n in package.namelist()
                       if PurePosixPath(n).as_posix().endswith("/" + filename) and not n.endswith("/")]
            if len(matches) != 1:
                raise ValueError(f"压缩包中未找到唯一的 {filename}")
            result[filename] = package.read(matches[0])
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default="wynemo/yiterm", help="GitHub owner/repo")
    parser.add_argument("--branch", help="默认使用仓库默认分支")
    parser.add_argument("--output", type=Path, default=Path.cwd(), help="输出目录，默认当前目录")
    args = parser.parse_args()
    if len(args.repo.split("/")) != 2 or any(not p or p in (".", "..") for p in args.repo.split("/")):
        parser.error("--repo 必须是 owner/repo")
    if not shutil.which("gh"):
        parser.error("请先安装 GitHub CLI (gh)，然后运行 gh auth login。")
    api = GitHub(args.repo)
    branch = args.branch or api.json("")["default_branch"]
    run, artifact = latest_artifact(api, branch)
    print(f"下载 {artifact['name']}\n构建：{run['html_url']}", flush=True)
    data = api.get(f"/actions/artifacts/{artifact['id']}/zip")
    digest = artifact.get("digest")
    if digest and digest.startswith("sha256:"):
        if hashlib.sha256(data).hexdigest() != digest.split(":", 1)[1]:
            raise ValueError("Actions 产物 SHA-256 校验失败")
    files = unpack(data)
    args.output.mkdir(parents=True, exist_ok=True)
    # Validate all files before replacing existing binaries.
    with tempfile.TemporaryDirectory(prefix=".yiterm-download-", dir=args.output) as staging:
        for name, content in files.items():
            Path(staging, name).parent.mkdir(parents=True, exist_ok=True)
            Path(staging, name).write_bytes(content)
        for name in files:
            destination = args.output / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            os.replace(Path(staging, name), destination)
            print(f"已保存 {destination.resolve()}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, IndexError, zipfile.BadZipFile,
            subprocess.TimeoutExpired) as error:
        print(f"下载失败：{error}", file=sys.stderr)
        sys.exit(1)
