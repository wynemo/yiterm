# YiTerm 技术与详细使用说明

Rust 原生终端原型，支持 macOS、Windows，并提供面向 CentOS 7 的 Linux x64 CI 构建。使用 `alacritty_terminal` 解析终端协议、`portable-pty` 启动本地 shell / ConPTY、egui 绘制窗口。

终端内嵌 Lilex 字体，无需系统安装；16px 字号配合 1.3 倍行高，使框线字符连续衔接。字体许可证见 [OFL.txt](assets/fonts/lilex/OFL.txt)。

字符显示按终端网格定位。回退字体的符号字形若宽于分配的一列或两列，会只在水平方向压缩到分配宽度，保留高度，避免覆盖相邻字符；普通字符和已适配网格的宽字符保持原样。绘制时统一隐藏 Unicode 17.0 的默认不可见字符（如变体选择符、零宽连接符、方向控制符和 emoji 标签），避免产生额外缺字方框；保留可见的组合音标与包围标记，原始文本、复制内容和终端列宽不受此显示过滤影响。终端内核识别 VS16 将文本符号切换为 emoji 的序列（例如 `⚠️`、`❤️`），为这些序列保留两列；即使字符与变体选择符分批到达也保持相同占位，并处理行尾换行与插入模式。当前渲染器不支持完整的复杂文字塑形或彩色 emoji 序列合成，连接序列中的可见字符仍按终端网格分别显示。

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

### Linux x64 / CentOS 7.8

Linux 发布包使用 X11 后端，目标为 x86_64、glibc 2.17；需要图形会话和可用的 OpenGL 驱动。解压 `yiterm-<版本>-linux-x64.tar.gz` 后运行 `./yiterm`，默认使用 `$SHELL`。如需指定 shell：`./yiterm --shell /bin/bash`。

CentOS 7 的运行依赖（使用系统已有的 yum 仓库或管理员配置的归档镜像）：

```sh
sudo yum install libX11 libXcursor libXi libXrandr libxcb libxkbcommon libxkbcommon-x11 mesa-libGL mesa-dri-drivers xdg-utils openssh-clients
```

构建依赖可参考 [Alacritty 的 CentOS/RHEL 7 说明](https://github.com/alacritty/alacritty/blob/master/INSTALL.md#centosrhel-7)，但本项目使用 egui，依赖不完全相同。在 Linux 上安装 Rust 和 C 编译工具后：

```sh
sudo yum install gcc gcc-c++ make pkgconfig libX11-devel libXcursor-devel libXi-devel libXrandr-devel libxcb-devel libxkbcommon-devel mesa-libGL-devel
cargo build --locked --release --target x86_64-unknown-linux-gnu --no-default-features --features x11
```

默认源码构建同时启用 X11 和 Wayland；CentOS 7 包只启用 `x11`，避免依赖其旧版 Wayland 桌面环境。不要在新发行版直接构建后假定兼容 CentOS 7。CI 在 Linux runner 中通过 `quay.io/pypa/manylinux2014_x86_64`（CentOS 7 / glibc 2.17）运行 `scripts/build_linux.sh`，检查 ELF 架构、GLIBC 符号版本和共享库解析，随后打包、生成 SHA-256 并上传 Actions artifact。独立的 Linux 测试 job 下载同一份 artifact、校验 SHA-256，执行日志、性能汇总、刷新、Unicode 测试及 Xvfb/Mesa 窗口启动检查。测试失败不会阻止下载已上传的二进制；正式 Release 仍要求所有平台构建和 Linux 测试通过。

Linux 使用本地补丁 `vendor/eframe`：优先创建默认 OpenGL 3.3 core 上下文；失败时先尝试 OpenGL 2.1 compatibility，再尝试上游 GLES 回退。旧版 Mesa / 远程桌面因此可使用 egui_glow 的 GLSL 1.20 渲染路径，避免直接进入 GLES 回退后出现 `GLXBadContextTag`。窗口测试要求窗口实际可见且已初始化 OpenGL 渲染器。

该方案保留系统 glibc，不替换它，也不随包携带显卡驱动。CI 容器共享 runner 的 Linux 内核，Xvfb 使用软件渲染；它不能替代 CentOS 7.8 的旧内核、真实显卡、输入法、剪贴板和桌面交互实机验证。Linux 构建与运行检查只在 Linux CI / Linux 设备执行，macOS 本机不运行 Linux 构建。

### Windows SSH 与图片上传

Windows 下，YiTerm 的私有 `ssh.exe` wrapper 使用内置 Rust SSH 客户端（`russh`）。登录时输入密码或密钥口令，图片上传通过同一条已认证连接的新 SFTP 通道完成，无需再次输入密码。也支持服务器的 keyboard-interactive 认证。密码不会保存到配置文件或日志。

仍需安装系统 OpenSSH：内置客户端通过 `ssh -G` 解析 SSH config 中的别名、HostName、User、Port、IdentityFile、ProxyJump 和 known_hosts 路径。`ProxyJump` 的跳板链路由系统 OpenSSH 建立，目标主机仍由内置客户端认证并复用连接上传图片；跳板机认证按系统 OpenSSH 的配置和提示处理。支持 OpenSSH 格式私钥文件和 Windows OpenSSH agent；`IdentitiesOnly=yes` 时仅尝试密钥文件。首次连接显示 SHA-256 主机指纹，确认后写入 known_hosts；主机密钥变化或被撤销时拒绝连接。`StrictHostKeyChecking` 支持 `ask`、`yes` 和 `accept-new`。握手/指纹确认和认证阶段各有 120 秒期限。`ssh -G` 可能去掉 known_hosts 路径列表的引号；多个绝对路径可以包含空格，无法判断分隔的相对路径列表会报错，应改用绝对路径或系统客户端。

内置客户端暂不支持 ProxyCommand、端口/agent/X11 转发、SSH 证书和远程命令。遇到这些配置会提示使用 `ssh --system <原有参数>`，显式调用系统 OpenSSH；`ssh -V`、`ssh -G` 和 `ssh -Q` 也直接调用系统客户端。系统客户端会话仍通过独立 scp 上传图片，因此密码认证的会话不能保证免交互上传。macOS 继续使用系统 OpenSSH 的 ControlMaster 连接复用。

内置上传单张限制为 50 MiB，远端文件名为 `/tmp/yiterm-drop-<UUID>.png`，创建权限为 `0600`。GUI 通过仅当前 Windows 用户可访问的命名管道请求当前分栏的 wrapper 上传，不传递密码；上传完成且目标进程仍有效才粘贴远端路径。失败或取消时尽力清理未完成文件；服务器断开时可能无法清理。

本机开发只运行当前平台的 `cargo test --locked`；Windows 控制台、命名管道、窗口缩放和登录后连续传图的集成测试由 Windows CI runner 执行，不在 macOS 上安装 Windows 工具链或交叉编译。

## 文字选择与复制

按住鼠标左键拖动选择文字，选区会高亮，松开后保留。macOS 使用 **Cmd+C** 复制；Windows / Linux 使用 **Ctrl+Shift+C** 或 **Ctrl+Insert**。**Ctrl+C** 保留为终端中断命令。

Windows 有选中文字时，右键点击该 pane 直接复制。通过右键或快捷键复制后，自动取消选区。应用启用终端鼠标上报时，右键按下和松开转发给应用（例如 Herdr 的右键菜单），不触发选区复制；按住 Shift 可绕过鼠标上报。

终端程序开启鼠标模式时，普通鼠标操作发送给程序；按住 **Shift** 拖动可选择文字。选择也支持滚动历史中的内容。

SSH 远端程序可通过 OSC 52 的 `c` 目标把文本复制到主机剪贴板，单次最多 1 MiB；YiTerm 不支持远端读取主机剪贴板。Windows 内置 SSH wrapper 在输出进入 ConPTY 前截获 OSC 52，通过当前用户 ACL 保护的命名管道交给主终端进程；剪贴板写入使用 YiTerm 主窗口作为 Win32 owner，并读回校验。日志记录管道就绪、收到的请求和写入结果，不记录剪贴板正文。

## 终端链接

自动识别终端文字中的 HTTP/HTTPS 链接（支持自动折行）和 OSC 8 超链接。悬停时为整个链接加下划线并显示目标地址；macOS 按住 **Cmd** 点击，Windows / Linux 按住 **Ctrl** 点击，在系统默认浏览器打开。普通拖动仍用于选择文字。仅打开 HTTP/HTTPS 地址。

## 多标签页

顶部标签栏点击 **+** 新建 tab，点击标签切换，点击 **×** 关闭。每个 tab 有独立的 shell / SSH 会话、终端输出和粘贴状态，后台 tab 持续运行。新 tab 复用启动时的 `--shell` 或 `--ssh` 参数；关闭最后一个 tab 时退出窗口。

| 操作 | macOS | Windows / Linux |
| --- | --- | --- |
| 新建 tab | Cmd+T | Ctrl+Shift+T |
| 关闭当前 tab | Cmd+W | Ctrl+Shift+W |
| 下一个 tab | Ctrl+Tab | Ctrl+Tab |
| 上一个 tab | Ctrl+Shift+Tab | Ctrl+Shift+Tab |

## 分栏

使用顶部 **Pane → Split Right** 把当前 pane 拆成左右两栏，**Split Down** 拆成上下两栏，可继续拆分任意 pane。新 pane 使用启动时的 shell / SSH 参数，并拥有独立会话。点击 pane 切换输入焦点，蓝色边框标记当前 pane；粘贴按钮和图片上传作用于当前 pane。

拖动分隔线调整大小。点击 pane 标题旁的 **×** 或选择 **Pane → Close Pane** 关闭当前 pane，剩余区域自动合并。shell 退出也只关闭对应 pane；关闭最后一个 pane 时关闭该 tab。tab 的 **×** 和关闭 tab 快捷键会关闭整个 tab 的所有 pane。

## About

通过顶栏最后的 **About** 菜单项查看版本和构建提交，打开文档、GitHub 或查看随程序使用的字体及组件许可证。正式构建显示提交链接，本地构建显示开发版本标记。点击 **Close**、窗口外部或按 **Esc** 关闭。

## 日志

统一使用 `tracing`，默认应用级别为 `info`、依赖库为 `warn`，仅输出到日志文件；Windows 启动时不再附带控制台窗口。主程序和内置 SSH wrapper 共用每天一个日志文件，PID 写在每行开头；顶部 **Tools → Open Log File** 会直接打开当天的文件。

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

文件名为 `yiterm.<UTC日期>.log`，包含 PID、UTC 时间、级别、线程、模块和事件字段。主程序使用 `tracing-appender` 统一写文件、按天轮转和清理历史文件；Windows SSH wrapper 通过带随机令牌认证的本机回环 IPC 转发日志，不打开日志文件。后台有界队列写入，正常退出时刷盘；队列满时丢弃日志，并在退出时报告数量。异常强制终止可能丢失尚未刷盘的日志。

按 UTC 天轮转，最多保留 `max_files` 个日期文件。旧版本的 `yiterm-...` 文件保留原位，新日志使用 `yiterm.` 前缀。此设置不限制单个文件大小。目录无法创建或文件无法打开时在状态栏提示；SSH wrapper 无法连接日志转发服务时会在终端显示错误。Windows 和 macOS CI 会实际运行日志及多进程转发集成测试。

不记录终端输出、用户输入、剪贴板正文和图片内容。Windows 内置 SSH wrapper 与主程序共用日志文件；SSH 配置解析失败时会记录具体失败项，便于排查 OpenSSH 配置。PTY、图片上传和粘贴也记录调用侧诊断信息。

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

## 空闲刷新与 CPU 占用

PTY 收到输出时会唤醒界面；空闲时每 250 毫秒检查一次进程退出、异步粘贴和 Windows 剪贴板请求，不再固定以约 60 FPS 重绘。光标闪烁按 500 毫秒的边界安排刷新。后台标签页仍会处理输出。窗口标题只在内容变化时发送更新，避免标题命令触发持续重绘。

Windows 性能验证：使用 Release 构建，启动后等待提示符稳定，在任务管理器观察空闲 CPU，并检查持续输出、后台标签页、光标闪烁、粘贴和关闭 shell 的响应。实际占用取决于设备和图形驱动，需要在 Windows 上测量。

### Windows 高 CPU 诊断

先在任务管理器展开 YiTerm 进程组，确认高占用的 PID。Process Explorer 的进程属性 → Threads 可以按 CPU 排序并查看最忙线程的 Stack。单次栈是瞬时快照；看到 `OPENGL32.dll` 只能说明当时经过图形路径，不能直接证明反复创建上下文或软件渲染。

在 PowerShell 中开启汇总诊断后启动新进程：

```powershell
$env:YITERM_PROFILE = "1"
.\yiterm.exe --log-filter "warn,yiterm=info"
```

保持终端空闲约 30 秒，然后用 **Tools → Open Log File** 打开日志，搜索 `yiterm::profile`。完成后关闭该实例，并执行 `Remove-Item Env:YITERM_PROFILE`，之后启动的实例恢复默认关闭状态。日志文件写入需保持启用（`logging.file = true`）。

启动日志包含构建版本、OpenGL vendor、renderer 和 version；每约 5 秒汇总 UI update 次数、update/会话轮询耗时、上一帧框架耗时（包括渲染，不包括等待垂直同步）、PTY 读取次数和字节数、窗口焦点以及 egui 报告的重绘请求调用位置。耗时是墙钟时间，不等于线程 CPU 时间；框架耗时包含 update，不能与 update 耗时相加。`updates_per_s` 是 update 调用频率，不是显示器实际呈现帧率。日志不记录终端内容、按键或剪贴板正文。

`process_cpu_core_percent` 通过两次采样的进程累计 CPU 毫秒差除以墙钟时间计算，仅统计 YiTerm 当前进程，100% 表示使用一个逻辑核，不是任务管理器的进程组百分比；与系统总 CPU 百分比比较时需考虑逻辑核数量。采样依赖 UI update：若图形调用长时间卡住，日志可能延迟，此时需要外部采样。

Windows CI 额外保存 `profiling-windows-<commit>` artifact，内含同次优化构建的 `yiterm.exe` 和 `yiterm.pdb`。使用这一对文件进行诊断，不要混用其他版本的符号。通过 WPR 录制约 30 秒 CPU Usage，在 WPA 的 **CPU Usage (Sampled)** 中按 PID、线程和调用栈展开；配置该 PDB 所在目录和微软符号服务器后分析热点。可对比正常窗口/最小化、光标闪烁开/关，以及远程桌面/本机桌面。Windows 构建和采样在 Windows runner 或 Windows 设备进行，macOS 不交叉编译。

## 发布流程

版本号同步更新 `Cargo.toml` 和 `Cargo.lock`，提交后创建对应的 `v<版本号>` tag，并用 `git push --atomic origin main v<版本号>` 一起推送。

只有分支推送触发 CI，tag 推送不单独触发。一次 main 流水线完成 macOS、Windows、Linux 构建与测试；全部通过后，如果当前提交有对应版本 tag，就直接使用本次构建产物发布 GitHub Release。构建的打包步骤输出当前提交的版本 tag，发布 job 检查 macOS / Windows 和 Linux 打包步骤输出的版本 tag 一致后运行；没有对应 tag 时跳过整个发布 job。若 tag 在 main 流水线结束后才推送，可在 main 上手动运行该工作流完成发布。

CI 固定使用 Rust `1.99.1`，避免浮动 `stable` 自动升级导致依赖缓存失效。升级工具链时同步修改 `.github/workflows/ci.yml` 中的 `toolchain`、Linux `RUST_TOOLCHAIN` 和 Linux 缓存键；升级后的首次构建需要重建缓存。缓存前移除 runner 预装的其他 Rust 工具链，避免缓存键受 runner 镜像更新影响。release 调试符号配置在工作流全局设置，缓存、编译和测试使用同一配置。
