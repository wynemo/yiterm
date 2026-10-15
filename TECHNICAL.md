# YiTerm 技术与详细使用说明

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

Windows 默认启动 `powershell.exe`；macOS 默认使用 `$SHELL`。`--shell` 接收可执行文件路径，不接收整条命令。可以在窗口内直接输入 `ssh user@host`，进入远端后照常粘贴图片；也可使用 `--ssh` 直接启动远端会话。SSH 目标使用 `~/.ssh/config` 中的别名或 `user@host`；端口、密钥等写入 SSH config。远端需要 POSIX shell、可写的 `/tmp` 和 SFTP 支持。

### Windows SSH 与图片上传

Windows 下，YiTerm 的私有 `ssh.exe` wrapper 使用内置 Rust SSH 客户端（`russh`）。登录时输入密码或密钥口令，图片上传通过同一条已认证连接的新 SFTP 通道完成，无需再次输入密码。也支持服务器的 keyboard-interactive 认证。密码不会保存到配置文件或日志。

仍需安装系统 OpenSSH：内置客户端通过 `ssh -G` 解析 SSH config 中的别名、HostName、User、Port、IdentityFile、ProxyJump 和 known_hosts 路径。`ProxyJump` 的跳板链路由系统 OpenSSH 建立，目标主机仍由内置客户端认证并复用连接上传图片；跳板机认证按系统 OpenSSH 的配置和提示处理。支持 OpenSSH 格式私钥文件和 Windows OpenSSH agent；`IdentitiesOnly=yes` 时仅尝试密钥文件。首次连接显示 SHA-256 主机指纹，确认后写入 known_hosts；主机密钥变化或被撤销时拒绝连接。`StrictHostKeyChecking` 支持 `ask`、`yes` 和 `accept-new`。握手/指纹确认和认证阶段各有 120 秒期限。`ssh -G` 可能去掉 known_hosts 路径列表的引号；多个绝对路径可以包含空格，无法判断分隔的相对路径列表会报错，应改用绝对路径或系统客户端。

内置客户端暂不支持 ProxyCommand、端口/agent/X11 转发、SSH 证书和远程命令。遇到这些配置会提示使用 `ssh --system <原有参数>`，显式调用系统 OpenSSH；`ssh -V`、`ssh -G` 和 `ssh -Q` 也直接调用系统客户端。系统客户端会话仍通过独立 scp 上传图片，因此密码认证的会话不能保证免交互上传。macOS 继续使用系统 OpenSSH 的 ControlMaster 连接复用。

内置上传单张限制为 50 MiB，远端文件名为 `/tmp/yiterm-drop-<UUID>.png`，创建权限为 `0600`。GUI 通过仅当前 Windows 用户可访问的命名管道请求当前分栏的 wrapper 上传，不传递密码；上传完成且目标进程仍有效才粘贴远端路径。失败或取消时尽力清理未完成文件；服务器断开时可能无法清理。

本机开发只运行当前平台的 `cargo test --locked`；Windows 控制台、命名管道、窗口缩放和登录后连续传图的集成测试由 Windows CI runner 执行，不在 macOS 上安装 Windows 工具链或交叉编译。

## 文字选择与复制

按住鼠标左键拖动选择文字，选区会高亮，松开后保留。macOS 使用 **Cmd+C** 复制；Windows / Linux 使用 **Ctrl+Shift+C** 或 **Ctrl+Insert**。**Ctrl+C** 保留为终端中断命令。

Windows 有选中文字时，右键点击该 pane 直接复制，不打开分栏菜单；没有选区时右键仍打开菜单。通过右键或快捷键复制后，自动取消选区。

终端程序开启鼠标模式时，普通鼠标操作发送给程序；按住 **Shift** 拖动可选择文字。选择也支持滚动历史中的内容。

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

## About

通过顶栏最后的 **About** 菜单项查看版本和构建提交，打开文档、GitHub 或查看随程序使用的字体及组件许可证。正式构建显示提交链接，本地构建显示开发版本标记。点击 **Close**、窗口外部或按 **Esc** 关闭。

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

## 光标设置

点击顶部菜单栏第一个入口 **Settings** 打开设置界面。**Cursor Style** 提供实心方块、下划线和竖线三种样式，**Cursor Blinking** 控制是否闪烁。默认使用闪烁的实心方块，方块覆盖的字符以反色显示。修改立即应用到所有标签页和分栏，也适用于后续新建的会话。点击 **Done**、窗口外部或按 **Esc** 关闭。

设置自动保存到当前配置文件（支持 `--config`），重启后恢复。保存失败时在顶部显示错误，当前窗口仍使用已选择的样式。保存时保留其他配置项，但 TOML 注释和排版会被重新生成。也可以直接编辑：

```toml
[cursor]
shape = "block" # block / underline / beam
blinking = true
```

光标样式与闪烁由此设置统一控制；终端程序仍可隐藏或显示光标。手动编辑配置后需重启生效。

## 主题设置

在 **Settings → Theme** 中选择 **Dark**（黑色，默认）或 **Light**（白色）。切换立即应用到所有标签页、分栏和设置窗口，不会重启会话，并自动保存到当前配置文件（支持 `--config`）。保存失败时顶部显示错误，当前选择仍然生效。旧配置缺少主题设置时使用 Dark。

```toml
[appearance]
theme = "dark" # 或 "light"
```

两种主题分别配置 ANSI 16 色；其余 256 色和程序指定的 RGB 真彩色保持原值。程序自行指定的背景颜色也会保留。手动编辑配置后需重启生效。
