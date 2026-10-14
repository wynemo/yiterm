# YiTerm

Rust 原生终端原型，支持 macOS 和 Windows。使用 `alacritty_terminal` 解析终端协议、`portable-pty` 启动本地 shell / ConPTY、egui 绘制窗口。

终端内嵌 Lilex 字体，无需系统安装；16px 字号配合 1.3 倍行高，使框线字符连续衔接。字体许可证见 [OFL.txt](assets/fonts/lilex/OFL.txt)。

## 运行

下载 GitHub Actions 最新成功的 Windows x64 构建（Python 3.9+ 和 GitHub CLI `gh`）：

```sh
gh auth login
python scripts/download_latest.py
```

首次运行前执行 `gh auth login`，脚本通过 `gh api` 复用登录状态，也支持 gh 的 `GH_TOKEN` / `GITHUB_TOKEN` 环境变量。脚本默认选择默认分支最新成功、未过期的 push 构建，校验 SHA-256 后把 `yiterm.exe`、`conpty.dll`、`OpenConsole.exe` 写入当前目录（覆盖同名文件）。可用 `--branch main` 指定分支，或用 `--output 路径` 指定目录。

安装 Rust stable；macOS 还需 Xcode Command Line Tools，Windows 需 MSVC C++ 构建工具。

```sh
cargo run --release
cargo run --release -- --shell /bin/zsh
cargo run --release -- --ssh my-server
```

Windows 默认启动 `powershell.exe`；macOS 默认使用 `$SHELL`。`--shell` 接收可执行文件路径，不接收整条命令。可以在窗口内直接输入 `ssh user@host`，进入远端后照常粘贴图片；也可使用 `--ssh` 直接启动远端会话。SSH 目标使用 `~/.ssh/config` 中的别名或 `user@host`；端口、密钥、跳板机等写入 SSH config。远端需要 POSIX shell、可写的 `/tmp` 和 scp/SFTP 支持。

## 多标签页

顶部标签栏点击 **+** 新建 tab，点击标签切换，点击 **×** 关闭。每个 tab 有独立的 shell / SSH 会话、终端输出和粘贴状态，后台 tab 持续运行。新 tab 复用启动时的 `--shell` 或 `--ssh` 参数；关闭最后一个 tab 时退出窗口。

| 操作 | macOS | Windows / Linux |
| --- | --- | --- |
| 新建 tab | Cmd+T | Ctrl+Shift+T |
| 关闭当前 tab | Cmd+W | Ctrl+Shift+W |
| 下一个 tab | Ctrl+Tab | Ctrl+Tab |
| 上一个 tab | Ctrl+Shift+Tab | Ctrl+Shift+Tab |

## 分栏

在 pane 内右键点击可选择 **Split Right** 或 **Split Down**，操作作用于右键所在的 pane。也可使用顶部 **Pane → Split Right** 把当前 pane 拆成左右两栏，**Split Down** 拆成上下两栏，可继续拆分任意 pane。新 pane 使用启动时的 shell / SSH 参数，并拥有独立会话。点击 pane 切换输入焦点，蓝色边框标记当前 pane；粘贴按钮和图片上传作用于当前 pane。

拖动分隔线调整大小。点击 pane 标题旁的 **×** 或选择 **Pane → Close Pane** 关闭当前 pane，剩余区域自动合并。shell 退出也只关闭对应 pane；关闭最后一个 pane 时关闭该 tab。tab 的 **×** 和关闭 tab 快捷键会关闭整个 tab 的所有 pane。

## 日志

统一使用 `tracing`，默认应用级别为 `info`、依赖库为 `warn`，仅输出到日志文件；Windows 启动时不再附带控制台窗口。顶部 **Tools → Open Log Directory** 菜单可打开实际日志目录。

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

过滤配置优先级：`--log-filter` > `YITERM_LOG` > TOML 中的 `logging.filter` > 默认值。配置修改后重启生效；无效配置会报告错误。可设置 `file = false` 关闭文件输出。旧配置中的 `console` 字段仍可读取，但不再启用控制台输出。

文件名为 `yiterm-<PID>.<UTC日期>.log`，包含 PID、UTC 时间、级别、线程、模块和事件字段。后台有界队列写入，正常退出时刷盘；队列满时丢弃日志，并在退出时报告数量。异常强制终止可能丢失尚未刷盘的日志。

按 UTC 天轮转，每个实例最多保留 `max_files` 个文件；启动时还清理已退出实例的旧日志，给新文件预留一个名额。其他活跃实例的文件保留，因此多个实例运行时总文件数可能超过该值。此设置不限制单个文件大小。目录无法创建或文件无法打开时在状态栏提示。

不记录终端输出、用户输入、剪贴板正文和图片内容。SSH wrapper 保持现有启动分流，不初始化 GUI 日志；PTY、图片上传和粘贴的调用侧记录诊断信息。
