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
