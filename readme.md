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

## 日志

统一使用 `tracing`，默认应用级别为 `info`、依赖库为 `warn`，同时输出到 stderr 和文件。顶部 **Tools → Open Log Directory** 菜单可打开实际日志目录。

默认日志目录：

- macOS：`~/Library/Logs/yiterm/`
- Windows：`%LOCALAPPDATA%\yiterm\logs\`
- Linux：`$XDG_DATA_HOME/yiterm/logs/`，未设置时为 `~/.local/share/yiterm/logs/`。

默认配置文件：macOS 为 `~/Library/Application Support/yiterm/config.toml`；Windows 为 `%APPDATA%\yiterm\config\config.toml`；Linux 为 `$XDG_CONFIG_HOME/yiterm/config.toml`（未设置时为 `~/.config/yiterm/config.toml`）。文件缺失时使用默认值，可参考仓库中的 `config.example.toml` 创建配置。

```sh
cargo run -- --config ./config.example.toml
cargo run -- --log-filter 'warn,yiterm=debug,yiterm::mouse=trace'
YITERM_LOG='warn,yiterm=debug' cargo run
```

过滤配置优先级：`--log-filter` > `YITERM_LOG` > TOML 中的 `logging.filter` > 默认值。配置修改后重启生效；无效配置会报告错误。可设置 `console = false` 或 `file = false` 分别关闭输出。

文件名为 `yiterm-<PID>.<UTC日期>.log`，包含 PID、UTC 时间、级别、线程、模块和事件字段。后台有界队列写入，正常退出时刷盘；队列满时丢弃日志，并在退出时报告数量。异常强制终止可能丢失尚未刷盘的日志。

按 UTC 天轮转，每个实例最多保留 `max_files` 个文件；启动时还清理已退出实例的旧日志，给新文件预留一个名额。其他活跃实例的文件保留，因此多个实例运行时总文件数可能超过该值。此设置不限制单个文件大小。目录无法创建或文件无法打开时回退到 stderr，并在状态栏提示。

不记录终端输出、用户输入、剪贴板正文和图片内容。SSH wrapper 保持现有启动分流，不初始化 GUI 日志；PTY、图片上传和粘贴的调用侧记录诊断信息。
