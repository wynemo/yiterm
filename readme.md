# YiTerm 0.0.1

Rust 原生终端原型，支持 macOS 和 Windows。使用 `alacritty_terminal` 解析终端协议、`portable-pty` 启动本地 shell / ConPTY、egui 绘制窗口。

## 运行

安装 Rust stable；macOS 还需 Xcode Command Line Tools，Windows 需 MSVC C++ 构建工具。

```sh
cargo run --release
cargo run --release -- --shell /bin/zsh
cargo run --release -- --ssh my-server
```

Windows 默认启动 `powershell.exe`；macOS 默认使用 `$SHELL`。`--shell` 接收可执行文件路径，不接收整条命令。可以在窗口内直接输入 `ssh user@host`，进入远端后照常粘贴图片；也可使用 `--ssh` 直接启动远端会话。SSH 目标使用 `~/.ssh/config` 中的别名或 `user@host`；端口、密钥、跳板机等写入 SSH config。远端需要 POSIX shell、可写的 `/tmp` 和 scp/SFTP 支持。

## 图片粘贴

- macOS：Cmd+V；Windows：Ctrl+Shift+V。底部 Paste 按钮也可使用。
- 剪贴板有图片时，优先保存为 PNG；只有文字时粘贴文字。
- 本地保存到系统临时目录的 `yiterm-drop-<uuid>.png`，macOS 通常为 `$TMPDIR`，Windows 为 `%TEMP%`。Unix 文件权限为 `0600`。
- 本地会注入经过 shell 转义的路径，不自动按 Enter。Windows 路径按 PowerShell 语法转义，macOS 按 POSIX shell 语法转义。
- 粘贴图片时自动识别当前 SSH 会话，先上传到远端 `/tmp`，成功后才注入远端路径。上传在后台执行，有 60 秒超时；失败显示在状态栏，不注入本地路径。上传期间退出或切换前台会话也不会误注入。
- macOS 在会话内通过私有 PATH 中的 SSH 包装程序为普通 `ssh` 自动添加 ControlMaster 参数，使用短路径、权限 `0700` 的私有目录保存 socket。保留用户显式传入的选项，不改写 `~/.ssh/config`。粘贴时按前台进程组和进程树读取真实 SSH 参数，保留端口、密钥、用户、跳板机及 ControlPath，退出 SSH 后自动恢复本地路径。Windows OpenSSH 不提供同样的 Unix socket 复用，使用独立 scp 连接，需配置密钥或 ssh-agent。上传启用 BatchMode，不会弹出密码交互。
- 临时图片会保留，便于命令稍后读取；本地和远端都需要自行清理。

支持 ANSI 颜色、光标、备用屏幕、终端尺寸同步、滚动历史、方向键、Ctrl 字母快捷键和 bracketed paste。关闭窗口会终止 shell/SSH 子进程。

## 当前边界

这是单窗口、单会话原型，尚未实现标签页、鼠标协议、文本选中复制、完整快捷键映射和字体配置。新窗口内直接输入 `ssh` 即可。更新前已建立的 SSH 连接需要退出并重新连接，才能自动建立复用 socket。使用绝对路径 `/usr/bin/ssh` 或绕过 PATH 的 shell 别名仍会被识别，但不会自动增加复用参数；未开启复用的连接上传需要密钥或 ssh-agent。Windows 使用进程树识别当前 SSH；若同时存在多个 SSH 子进程，会拒绝图片上传以避免选错目标。远端再次 SSH 到另一台机器、tmux 内多个独立前台会话尚不支持自动确定最终目标。Windows 自定义 shell 若不是 PowerShell，图片路径的转义规则需要相应调整。默认字体对 CJK 字符的覆盖有限。

## 验证

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

上述检查可在本地按需运行；CI 只构建和打包。本地测试覆盖终端解析、查询响应、键盘序列、PNG 保存、路径转义、真实 PTY 中普通 SSH 的识别/退出恢复、sh/bash/zsh/fish 包装程序、上传参数、失败处理和超时；真实剪贴板与远端 SSH 上传仍需在目标环境验证。

`vendor/egui-winit` 基于 egui-winit 0.33.3（MIT OR Apache-2.0），增加 `terminal-input` feature，绕过框架的复制/粘贴快捷键拦截，确保 Ctrl+C 和纯图片粘贴能交给终端处理。升级 egui 时需保留此补丁。

## GitHub Actions 构建

推送代码、提交 PR 或在 Actions 页面手动运行 `Build YiTerm`，都会构建两个目标：

| 平台 | Rust target | Runner | 产物 |
| --- | --- | --- | --- |
| Windows x64 | `x86_64-pc-windows-msvc` | `windows-2022` | `yiterm-0.0.1-windows-x64.zip` |
| macOS arm64 | `aarch64-apple-darwin` | `macos-14` | `yiterm-0.0.1-macos-arm64.tar.gz` |

每个目标仅执行 release 构建、打包和产物上传，不运行格式检查、Clippy 或测试。成功后在本次运行的 Artifacts 下载对应平台产物；产物内附 SHA-256 校验文件，保留 30 天。打包脚本读取 Cargo.toml 中的版本号并检查 PE / Mach-O 的真实架构，macOS 压缩包保留可执行权限。

解压后运行 `yiterm.exe`（Windows）或 `./yiterm`（macOS）。SSH 功能需要系统 OpenSSH。构建产物目前是独立可执行文件，未提供安装器或签名公证。
