# YiTerm 技术与详细使用说明

Rust 原生终端，Windows / macOS 使用 GPUI 0.2.2，Linux 保留原有 egui / eframe / OpenGL 界面后端，使用 `alacritty_terminal` 解析终端协议、`portable-pty` 启动本地 shell / ConPTY。支持 macOS、Windows，Linux CI 保留 CentOS 7 / glibc 2.17 的 x64 兼容目标。

终端内嵌 Lilex 字体；界面内嵌 IBM Plex Sans Regular 字体（Zed 的 `.ZedSans` 别名所对应的字体，13px、字重 400；[许可证](assets/fonts/ibm-plex-sans/OFL.txt)），无需系统安装；终端默认 16px 字号配合 1.3 倍行高。macOS / Windows 将常用单线框线字符 `│─┌┐└┘├┤┬┴┼` 按终端单元格直接绘制并对齐物理像素，保证 Herdr 等终端程序的框线跨行、跨列连续衔接，不依赖所选字体的字形高度；滚动条使用的实心块、左右分数块和上下分数块（如 `█▕▐▏▀▄`）同样按单元格绘制。复制仍保留原始字符。其他框线字符和 Linux 仍使用字体绘制。字体许可证见 [OFL.txt](assets/fonts/lilex/OFL.txt)。

字符显示按终端网格定位，Windows / macOS 由 GPUI 排版每个单元格中的完整 Unicode 序列，保留变体选择符、连接符与组合标记，字体回退由平台处理。超出单元格分配宽度的字形会被裁剪，不会覆盖相邻字符；原始文本与复制内容保持原样。终端内核识别 VS16 将文本符号切换为 emoji 的序列（例如 `⚠️`、`❤️`），为这些序列保留两列；字符与变体选择符分批到达时也保持相同占位。跨多个独立终端单元格的复杂文字排版仍受字符网格限制。Linux 保持原有 egui 绘制逻辑：过滤默认不可见字符，保留组合标记，回退字形过宽时只在水平方向压缩；原始复制内容不变。

## 运行

### 项目图标

图标以汉字“驿”为主体，使用深靛色圆角底、浅色字形和薄荷绿终端光标。资源位于 `assets/icons/`：`source.png` 为原始设计图，`yiterm.png` 为 1024×1024 透明背景图，`yiterm.ico` 包含 Windows 常用的 16、32、48、64、128、256 像素尺寸，`yiterm.icns` 包含 macOS 的标准及 Retina 尺寸。Windows 构建通过 `build.rs` 将 ICO 嵌入可执行文件的图标资源 ID 1，GPUI 使用同一资源作为窗口和任务栏图标。Linux 将 PNG 内嵌为窗口图标（Wayland 桌面是否显示取决于合成器）。macOS 发布包额外包含 `YiTerm.app`，其 `Info.plist` 引用 ICNS，Finder 和 Dock 使用该图标；打包时进行 ad-hoc 签名并验证，未进行 Apple 公证。解压后可双击应用或将其拖入“应用程序”。保留包根目录的 `yiterm` 供命令行及现有自动更新流程使用；`.app` 内运行时暂不支持自动更新，需要下载新版替换应用。

下载 GitHub Actions 最新可用的 Windows x64 和 Linux x64 构建（Python 3.9+ 和 GitHub CLI `gh`）：

```sh
gh auth login
python scripts/download_latest.py
```

首次运行前执行 `gh auth login`，脚本通过 `gh api` 复用登录状态，也支持 gh 的 `GH_TOKEN` / `GITHUB_TOKEN` 环境变量。脚本默认同时下载 Windows x64 和 Linux x64，分别选择默认分支最新有对应平台产物、未过期的 push 构建。校验 SHA-256 后，把 Windows 的 `yiterm.exe`、`bin/ssh.exe`、`conpty.dll`、`OpenConsole.exe` 和 Linux 的 `yiterm` 写入当前目录（覆盖同名文件），并为 Linux 程序设置可执行权限（`0755`）。所有请求的平台下载、校验完成后才开始替换现有文件。可用 `--branch main` 指定分支，或用 `--output 路径` 指定目录。

安装 Rust stable；macOS 还需 Xcode Command Line Tools，Windows 需 MSVC C++ 构建工具。

```sh
cargo run --release
cargo run --release -- --shell /bin/zsh
cargo run --release -- --ssh my-server
```

在 **Settings → Shell** 中选择 **Default**（默认）或 **Program**。Default 在 Windows 使用 `powershell.exe`，macOS/Linux 使用 `$SHELL`（未设置时使用 `/bin/sh`）。选择 Program 后，在 **Program** 输入可执行文件名或完整路径，例如 `pwsh.exe`、`cmd.exe`、`/bin/zsh`；路径包含空格时无需加引号，不接收整条命令或额外参数。输入后按 Enter、离开输入框或点击 Done 提交保存。程序名不能为空；不存在或无法执行时，新建终端会显示启动错误。

Shell 下拉列表会列出当前进程 PATH 中发现的常见 Shell，包括当前平台可用的 PowerShell、CMD、Bash、Zsh、Fish、Git Bash、Mintty、WSL、Nushell 等；支持搜索，每次打开 Settings 时刷新。Windows / macOS 的列表加宽并随窗口宽度限制，优先显示程序名，旁边显示可省略的路径以区分多个安装；悬停列表项或当前选项可查看完整路径，搜索和保存仍使用完整路径。Windows 上，若 PATH 仅包含 Git 的 `cmd` 目录，还会检查同一安装目录下的 Git Bash、Bash 和 Mintty。未列出的程序可以手动输入，或点击 **Browse…** 打开独立的系统文件选择窗口；取消不会改变设置。Linux 在 GNOME 等桌面优先调用 `zenity` 的 GTK 文件对话框，在 KDE 优先调用 `kdialog`，首选工具未安装时尝试另一种。选择窗口在后台进程中运行，不阻塞终端；同一时间只能打开一个 Shell 文件选择窗口。选定后检查是否为可执行文件，通过后自动填入 Program 并保存。

Linux 的 Browse 需要安装 `zenity` 或 `kdialog`。如两者均未安装，设置页会提示安装，不再显示内嵌文件列表；仍可手动输入完整路径。CentOS 7 可运行 `sudo yum install zenity`，Ubuntu 可运行 `sudo apt install zenity`。对话框使用系统自带的 GTK / Qt 环境，YiTerm 不新增 GTK / Qt 编译依赖或提高 glibc 最低版本。

Linux/macOS 还扫描 `/bin`、`/usr/bin`、`/usr/local/bin`，并读取 `$SHELL` 和 `/etc/shells` 中存在且可执行的程序，避免桌面启动时 PATH 不完整导致漏项。Linux 的 Program 输入支持常规复制、剪切和粘贴；清空输入时显示错误并保留此前有效设置，有效内容在结束编辑时保存。Linux 设置弹窗和搜索框持有输入焦点时，后台终端不接收键盘输入，也不覆盖输入框的输入法光标位置。

Windows 选择 `git-bash.exe` 或 `mintty.exe` 时，会查找同一安装目录下的 `bash.exe`，在 YiTerm 内启动 Bash；找不到时显示错误并提示直接选择 `bash.exe`。它们自己的终端窗口、主题和启动参数不用于 YiTerm。

设置自动保存到当前配置文件（支持 `--config`），用于下次启动及之后新建的本地 tab / pane，已有会话继续运行。`--shell` 优先于此设置，`--ssh` 仍直接连接远端。配置示例：

```toml
[shell]
mode = "program" # default / program
program = "pwsh.exe"
```

Windows 默认启动 `powershell.exe`；macOS 默认使用 `$SHELL`。`--shell` 接收可执行文件路径，不接收整条命令。可以在窗口内直接输入 `ssh user@host`，进入远端后照常粘贴图片；也可使用 `--ssh` 直接启动远端会话。SSH 目标使用 `~/.ssh/config` 中的别名或 `user@host`；端口、密钥等写入 SSH config。远端需要 POSIX shell、可写的 `/tmp` 和 SFTP 支持。

### Linux x64 / CentOS 7.8

Linux 启动时通过 Fontconfig 的 `fc-match` 加载系统中日韩字体回退，并保留 TTC 字体集合的 face index；终端与界面均使用这些回退字体，终端 ASCII 优先使用内置 Lilex，界面使用内嵌 IBM Plex Sans。需要系统安装 Fontconfig 和中文字体（Ubuntu 通常已安装），无需把字体复制到远程 shell 所在机器。

Linux 打包时从可执行文件分离 DWARF 调试符号，下载包只包含精简后的程序。CI 另外保存 `profiling-linux-<commit>` artifact，其中的 `yiterm` 和 `yiterm.debug` 必须配套使用；放在同一目录时 GDB 可通过 `.gnu_debuglink` 加载符号。原始 Cargo 编译产物保留，避免影响构建缓存。

Linux 发布包使用 X11 后端，目标为 x86_64、glibc 2.17；需要图形会话和可用的 OpenGL 驱动。解压 `yiterm-<版本>-linux-x64.tar.gz` 后运行 `./yiterm`，默认使用 `$SHELL`。如需指定 shell：`./yiterm --shell /bin/bash`。

#### 从应用菜单或桌面启动

Linux 包附带 `install-desktop.sh` 和 `yiterm.png`。先把整个解压目录放到长期保留的位置，再在该目录运行一次（无需 sudo）：

```sh
bash ./install-desktop.sh
```

脚本恢复当前用户的程序执行权限，并把 `yiterm.desktop` 写入 `$XDG_DATA_HOME/applications/`（默认 `~/.local/share/applications/`）。之后在应用菜单搜索 **YiTerm** 并点击启动，无需先打开终端。入口直接引用解压目录中的程序和图标；移动目录后需重新运行脚本，原目录应保持可写以便自动更新。脚本不会复制程序或修改系统 PATH。

如需桌面快捷方式，可将生成的 `yiterm.desktop` 复制到系统的桌面目录，在文件属性中允许执行；部分桌面还需要右键选择“允许启动”或“信任”。桌面快捷方式的支持和信任提示取决于桌面环境。卸载菜单入口时删除上述 `yiterm.desktop` 即可。

直接双击裸 ELF 文件是否运行取决于文件管理器策略；能从终端运行并不保证能直接双击。若提示无执行权限，可在文件属性的权限页允许作为程序执行，或运行 `chmod u+x ./yiterm`。优先使用保留 Unix 权限的官方 `.tar.gz` 包；自行转存为 ZIP 或经过其他文件系统后可能丢失权限。旧版本或 `scripts/download_latest.py` 只取得裸程序时，可手动创建同样的 `.desktop` 入口，或重新下载包含安装脚本的完整发布包。

CentOS 7 的运行依赖（使用系统已有的 yum 仓库或管理员配置的归档镜像）：

```sh
sudo yum install libX11 libXcursor libXi libXrandr libxcb libxkbcommon libxkbcommon-x11 mesa-libGL mesa-dri-drivers xdg-utils openssh-clients
```

构建依赖可参考 [Alacritty 的 CentOS/RHEL 7 说明](https://github.com/alacritty/alacritty/blob/master/INSTALL.md#centosrhel-7)，但本项目使用 egui，依赖不完全相同。在 Linux 上安装 Rust 和 C 编译工具后：

```sh
sudo yum install gcc gcc-c++ make pkgconfig libX11-devel libXcursor-devel libXi-devel libXrandr-devel libxcb-devel libxkbcommon-devel mesa-libGL-devel
cargo build --locked --release --target x86_64-unknown-linux-gnu --no-default-features --features x11
```

默认源码构建同时启用 X11 和 Wayland；CentOS 7 包只启用 `x11`，避免依赖其旧版 Wayland 桌面环境。不要在新发行版直接构建后假定兼容 CentOS 7。CI 在 Linux runner 中通过 `quay.io/pypa/manylinux2014_x86_64`（CentOS 7 / glibc 2.17）运行 `scripts/build_linux.sh`，检查 ELF 架构、GLIBC 符号版本和共享库解析，随后打包、生成 SHA-256 并上传 Actions artifact。

Linux 使用本地补丁 `vendor/eframe`：优先创建默认 OpenGL 3.3 core 上下文；失败时先尝试 OpenGL 2.1 compatibility，再尝试上游 GLES 回退。旧版 Mesa / 远程桌面因此可使用 egui_glow 的 GLSL 1.20 渲染路径，兼容不支持 3.3 core 的驱动。`vendor/glutin` 使用 `glXQueryExtensionsString` 查询当前连接与屏幕实际可用的 GLX 扩展，避免把客户端库支持的扩展误认为 X Server 也支持，从而在设置垂直同步时触发异步 `GLXBadContextTag`。

该方案保留系统 glibc，不替换它，也不随包携带显卡驱动。CI 容器共享 runner 的 Linux 内核，Xvfb 使用软件渲染；它不能替代 CentOS 7.8 的旧内核、真实显卡、输入法、剪贴板和桌面交互实机验证。Linux 构建与运行检查只在 Linux CI / Linux 设备执行，macOS 本机不运行 Linux 构建。

### CI 构建

Windows、macOS 和 Linux CI 只执行 Release 构建、打包校验和产物上传，不编译或运行测试。构建产物上传后即可下载；正式 GitHub Release 在所有平台构建成功且版本 tag 匹配后发布。

Windows / macOS CI 保留 Cargo registry、Git 依赖及完整 Release 编译产物，避免缓存清理移除 `vendor/gpui`、`vendor/alacritty_terminal` 等本地依赖后每次重新编译。缓存键包含 runner、目标架构、Cargo 清单与锁文件、全部 vendor 内容、Cargo 配置及工作流（含固定工具链和构建参数），只接受精确匹配。CI 在构建前固定 vendor 文件时间戳，避免 checkout 时间变化使未修改的依赖失效；vendor 内容变化时使用新缓存重新构建。精确命中成功构建保存的缓存后，Cargo 使用离线模式，省去 crates.io 索引更新；未命中时正常联网获取依赖。首次构建或依赖、工作流变化后的构建需要重新填充缓存；实际提速以之后 Windows CI 的构建耗时为准。

### Windows SSH 与图片上传

Windows 发布包携带独立的控制台程序 `bin/ssh.exe`，所有标签页和分栏共用该文件，各连接运行独立进程。YiTerm 将随包 `bin` 加入其子 shell 的 PATH 最前面，输入 `ssh host` 即调用内置客户端；`--ssh` 直接启动该程序。不修改系统 PATH，不使用 shell 函数、别名或转发脚本。会话临时目录只保存会话数据和必要的 shell 初始化脚本，不复制 exe，也不修改 PE 头；不要单独移动主程序或删除 `bin/ssh.exe`。从源码运行 Windows 版本前执行 `cargo build --locked --release --bins`，Cargo 输出目录中的 `ssh.exe` 可供同目录的主程序使用。

Windows 下，独立 `ssh.exe` 使用内置 Rust SSH 客户端（`russh`）。登录时输入密码或密钥口令，图片上传通过同一条已认证连接的新 SFTP 通道完成，无需再次输入密码。也支持服务器的 keyboard-interactive 认证。密码不会保存到配置文件或日志。

仍需安装系统 OpenSSH：内置客户端通过 `ssh -G` 解析 SSH config 中的别名、HostName、User、Port、IdentityFile、ProxyJump 和 known_hosts 路径。`ProxyJump` 的跳板链路由系统 OpenSSH 建立，目标主机仍由内置客户端认证并复用连接上传图片；跳板机认证按系统 OpenSSH 的配置和提示处理。支持 OpenSSH 格式私钥文件和 Windows OpenSSH agent；`IdentitiesOnly=yes` 时仅尝试密钥文件。首次连接显示 SHA-256 主机指纹，确认后写入 known_hosts；主机密钥变化或被撤销时拒绝连接。`StrictHostKeyChecking` 支持 `ask`、`yes` 和 `accept-new`。握手/指纹确认和认证阶段各有 120 秒期限。`ssh -G` 可能去掉 known_hosts 路径列表的引号；多个绝对路径可以包含空格，无法判断分隔的相对路径列表会报错，应改用绝对路径或系统客户端。

内置客户端暂不支持 ProxyCommand、端口/agent/X11 转发、SSH 证书和远程命令。遇到这些配置会提示使用 `ssh --system <原有参数>`，显式调用系统 OpenSSH；`ssh -V`、`ssh -G` 和 `ssh -Q` 也直接调用系统客户端。系统客户端会话仍通过独立 scp 上传图片，因此密码认证的会话不能保证免交互上传。macOS 继续使用系统 OpenSSH 的 ControlMaster 连接复用。

内置上传单张限制为 50 MiB，远端文件名为 `/tmp/yiterm-drop-<UUID>.png`，创建权限为 `0600`。GUI 通过仅当前 Windows 用户可访问的命名管道请求当前分栏的 wrapper 上传，不传递密码；上传完成且目标进程仍有效才粘贴远端路径。失败或取消时尽力清理未完成文件；服务器断开时可能无法清理。

### Windows 内置浏览器与 SSH SOCKS5

Windows 的 **Tools → Open SSH Browser** 会从当前聚焦 pane 检测前台 SSH 进程，并通过该 YiTerm 内置 SSH wrapper 已认证的连接启动 WebView2。浏览器显示在当前 tab 内容的右侧，终端中的 HTTP/HTTPS 链接按住 **Ctrl** 点击默认也会在这里打开。浏览器工具栏可前进、后退、刷新和输入网址；没有协议的地址按 HTTPS 处理。点击 **Open in System Browser** 可在系统默认浏览器打开当前页面（仅 HTTP/HTTPS，空白页不可用）；系统浏览器使用自身的网络与登录状态，不会继承内置浏览器的 SSH SOCKS 连接或 Cookie。拖动终端与浏览器之间的竖向分隔条可左右调整宽度。浏览器顶部 **+** 新建空白标签页，点击标签切换，标签旁的 **×** 关闭该页；**Close** 关闭整个浏览器并恢复终端宽度，关闭最后一个浏览器标签页也会关闭浏览器。各浏览器标签页保留各自的页面、地址和导航历史，共享同一 SSH SOCKS 连接和临时浏览器配置目录（包括 Cookie / 登录状态）。同一 SSH 会话中再次通过菜单或 Ctrl 点击终端链接，会增加浏览器标签页；网页请求打开新窗口的 HTTP/HTTPS 链接也会转为标签页。切换到其他终端 tab 时，浏览器随所属 tab 隐藏；切回后继续显示。

主程序通过受当前 Windows 用户 ACL 保护的 SSH 私有命名管道请求 SOCKS5 监听端口，只传递开始/结束和状态，不传递 SSH 凭据。浏览器侧使用无认证 SOCKS5，不需要单独填写 SOCKS 用户名或密码；SSH 凭据仍按原 SSH 登录流程处理。监听地址仅为 `127.0.0.1`，只支持 TCP CONNECT；目标主机名（包括 `localhost`）随 SSH `direct-tcpip` 通道发送，由远端解析 DNS。每个网页 TCP 连接复用同一条已认证 SSH transport 上的新 SSH channel。关闭整个浏览器或所属终端 tab 会停止该浏览器的 SOCKS 监听，SSH 会话退出时也会关闭。WebView2 使用单独的临时用户数据目录，并通过 Chromium 参数关闭 localhost 的隐式代理绕过、限制本地 DNS 解析、禁用 QUIC 和非代理 WebRTC UDP。页面导航失败时，浏览器区域会显示 WebView2 错误码，日志会记录详情。

服务器必须允许 TCP 转发（`AllowTcpForwarding yes`）。远端拒绝转发时浏览器显示连接失败，不会回退到本机直连。SOCKS5 不支持 UDP；浏览器参数禁用 QUIC 和非代理 WebRTC UDP，因此这些基于 UDP 的功能不可用。该功能要求 Microsoft Edge WebView2 Runtime，以及 YiTerm 内置 Windows SSH 客户端；显式使用 `ssh --system` 或普通系统 OpenSSH 时，YiTerm 无法取得其已认证连接，会提示在内置 SSH 会话中重试。Windows 的 **Settings → Open Links in Built-in Browser** 默认开启；关闭后，Ctrl 点击终端链接会在系统默认浏览器打开，不要求当前存在 SSH 会话。此设置立即生效并自动保存，重启后恢复；**Tools → Open SSH Browser** 仍可手动打开 SSH 内置浏览器。配置对应：

```toml
[browser]
open_links_in_browser = true
```

Chromium 的 `host-resolver-rules` 同样会作用于代理入口的 IP 地址，因此阻止本地 DNS 时必须保留 `EXCLUDE 127.0.0.1`，使本地 SOCKS 监听端口仍可连接。

Windows 的 GPUI DirectComposition 合成层放在原生子窗口下方，让 WebView2 子窗口能显示在终端界面之上。这项修正在 `vendor/gpui` 中维护（基于 GPUI 0.2.2）；上游的 `topmost = true` 会让不透明 GPUI 背景覆盖浏览器，造成请求成功、DOM 已加载但仍看到空白。Windows CI 同时打开真实 GPUI 窗口与 WebView2，并检查屏幕像素，验证浏览器内容没有被覆盖。

浏览器和 VNC 共用的顶部标签栏提供 **Maximize / Restore**：点击 **Maximize** 后，面板占满应用窗口内容区，隐藏终端、主菜单、终端标签栏和底部状态栏；面板自身的标签栏与工具栏继续显示，可切换页面或点击 **Restore** 恢复原来的左右分栏宽度。切换大小不会重新连接 SSH、VNC 或重新加载网页；最大化时键盘输入不会发送到隐藏的终端。点击 **Close** 或关闭最后一个页面也会恢复终端界面。

### Windows 内置 VNC

在已登录的内置 Windows SSH 会话中选择 **Tools → Open SSH VNC**，右侧会增加一个 **SSH VNC** 标签。填写 Host 和 Port 后点击 **Connect / Reconnect**；默认 `localhost:5901`，这里的 `localhost` 指 SSH 服务器。可连接 SSH 服务器可访问的其他 VNC 主机。目标主机须已有运行中的 VNC 服务，SSH 服务须允许 TCP 转发；YiTerm 不负责启动远端桌面服务。

VNC 标签与浏览器共用右侧标签栏和已认证 SSH transport；每个 VNC 标签拥有独立连接、本机桥接端口和 WebView2 临时配置目录。通过菜单可增加多个 VNC 标签，切换终端 tab 时连接保留，关闭 VNC 标签会关闭桥接及活动连接。SSH 断开后显示断连状态；重新登录 SSH 后应从菜单新建 VNC 标签。系统 OpenSSH / `ssh --system` 会话无法共享连接。需要 Microsoft Edge WebView2 Runtime。

VNC 页面右上角的 **Hide controls / Show controls** 可隐藏或显示 Host、Port、连接操作、工具按钮和状态信息。默认显示；收起后只保留右上角的恢复按钮，远端桌面使用腾出的空间。切换不会断开连接或重置输入与选项；**Fit** 开启时桌面随可用区域自动缩放。

**Screen** 提供 **All / Screen 1 / Screen 2 / …** 按钮，连接后根据 VNC 服务端返回的真实屏幕列表生成编号，点击即可切屏，当前选中按钮高亮，无需框选。标准 VNC 使用 ExtendedDesktopSize 布局在本地裁剪显示，鼠标坐标随选区映射，服务端仍发送完整桌面。macOS Screen Sharing 保留 Apple `003.889` 协议版本，以增强模式初始化（ClientInit 位 7；不设置会话选择位 6），发送 ViewerInfo、SetMode、SetDisplay，并启用自动画面更新；同时处理 DisplayInfo（1101）、DisplayInfo2（1105 / `0x451`）及独立的 `0x51` 屏幕列表消息。Mac 单屏切换使用服务端返回的真实显示器 ID 发送 SetDisplay，服务端调整当前连接的画面大小，不修改远端显示器配置，也不创建虚拟桌面。屏幕编号按组合画面从左到右、从上到下排列。不通过 SSH 执行命令获取屏幕信息。协议参考见 [noVNC-ARD](https://github.com/peetinc/noVNC-ARD/blob/main/ard/ard-patch.js) 和 [DisplayInfo2 格式](https://github.com/peetinc/noVNC-ARD/blob/main/ard/display-info.js)。服务端没有提供屏幕列表时会显示提示，不按画面均分生成屏幕。Windows WebView2 与实际 macOS VNC 服务端的互通效果尚待实机验证。

连接和屏幕列表更新时，应用日志写入 `VNC protocol diagnostic`，仅记录 Apple 协议标志、认证类型编号、画面尺寸、屏幕数量和当前屏幕 ID，便于区分协商与屏幕消息处理问题；不记录密码、键鼠内容或远端画面。通过 **Tools → Open Log File** 可查看。

远端未提供光标图像或发送透明光标时，显示本地小圆点作为鼠标定位提示；收到可见光标图像后使用远端形状。支持键鼠操作、**Fit** 缩放、**View only** 只读模式、**Ctrl+Alt+Del**、手动断开和重连。**Send clipboard text** 弹出文本框，将输入的文本发送到远端剪贴板；远端剪贴板更新时弹出可复制的文本框。剪贴板为文本传输，不自动读取或改写系统剪贴板。VNC 凭据在需要时通过认证框输入，密码字段隐藏内容，不保存到 YiTerm 配置或日志。具体认证和剪贴板能力取决于服务器与 noVNC 的兼容情况；不提供音频或文件传输。

内置 [noVNC v1.7.0](https://github.com/novnc/noVNC/tree/v1.7.0) 原始资源位于 `assets/novnc/`，构建时嵌入程序，无需 CDN、Node.js 或远端 websockify。许可证与来源见 [LICENSE.txt](assets/novnc/LICENSE.txt) 和 [UPSTREAM.txt](assets/novnc/UPSTREAM.txt)。本机 Rust 桥接仅监听 `127.0.0.1` 随机端口，以随机会话令牌和 WebSocket Origin 检查限制访问；VNC WebView 禁用代理并限制在内置页面导航，页面的 CSP 限制资源和网络请求。桥接通过现有 SOCKS5 将目标域名交给 SSH 服务器解析，不回退本机直连。SSH 保护到 SSH 服务器的链路，SSH 服务器到另一台 VNC 主机之间的加密取决于 VNC 服务配置。

Windows 的焦点、输入法、实际服务器认证和高 DPI 仍需实机兼容性验证。

源码构建使用当前平台的 Release 模式；不在 macOS 上安装 Windows 工具链或交叉编译。


Linux 不引入 GPUI、Vulkan 或新版 glibc 要求。上述兼容路径和验证脚本从原版保留，本次分流改动仍需 Linux CI 实际构建与运行确认。
## 文字选择与复制

按住鼠标左键拖动选择文字，选区会高亮，松开后保留。macOS 使用 **Cmd+C** 复制；Windows / Linux 使用 **Ctrl+Shift+C** 或 **Ctrl+Insert**。**Ctrl+C** 保留为终端中断命令。

在终端内容区右键打开 **Copy / Paste** 菜单，操作右键所在的 pane。打开菜单保留选区；无选中文字时 Copy 置灰，复制后自动取消选区。Paste 复用快捷键的文本粘贴、图片处理与上传逻辑，处理期间置灰。菜单在窗口边缘自动调整位置；点击菜单外或按 **Esc** 关闭并恢复终端输入，关闭操作不发送给终端。

应用启用终端鼠标上报时，普通右键按下和松开转发给应用（例如 Herdr 的右键菜单）；按住 **Shift + 右键** 打开 YiTerm 的 Copy / Paste 菜单。

Windows 内置 SSH 在进入认证和终端转发时清除继承的 Win32 `ENABLE_MOUSE_INPUT`，避免关闭 Quick Edit 后由 ConPTY 自动发出 `1003/1006` 指令，使普通 SSH shell 意外接管选字和右键。远端程序主动发送的 VT 鼠标模式指令仍正常处理；SSH 退出时恢复父 shell 的原始控制台模式。默认 INFO 日志中的 `SSH console input configured ...` 记录输入模式及 `mouse_input=false`，无需开启 DEBUG 即可确认这一初始化步骤。

终端程序开启鼠标模式时，普通鼠标操作发送给程序；按住 **Shift** 拖动可选择文字。选择也支持滚动历史中的内容。

SSH 远端程序可通过 OSC 52 的 `c` 目标把文本复制到主机剪贴板，单次最多 1 MiB；YiTerm 不支持远端读取主机剪贴板。Windows 内置 SSH wrapper 在输出进入 ConPTY 前截获 OSC 52，通过当前用户 ACL 保护的命名管道交给主终端进程；剪贴板写入使用 YiTerm 主窗口作为 Win32 owner，并读回校验。日志记录管道就绪、收到的请求和写入结果，不记录剪贴板正文。

## 终端链接

自动识别终端文字中的 HTTP/HTTPS 链接（支持自动折行）和 OSC 8 超链接。悬停时为整个链接加下划线并显示目标地址；macOS 按住 **Cmd** 点击、Linux 按住 **Ctrl** 点击，在系统默认浏览器打开；Windows 按住 **Ctrl** 点击时，默认使用“Windows 内置浏览器与 SSH SOCKS5”所述的当前聚焦 SSH pane；关闭 **Open Links in Built-in Browser** 设置后使用系统默认浏览器。普通拖动仍用于选择文字。仅打开 HTTP/HTTPS 地址。

## 多标签页

顶部标签栏点击 **+** 新建 tab，点击标签切换，点击 **×** 关闭。每个 tab 有独立的 shell / SSH 会话、终端输出和粘贴状态，后台 tab 持续运行。新 tab 使用当前 Shell 设置；启动时指定的 `--shell` 或 `--ssh` 参数优先；关闭最后一个 tab 时退出窗口。

| 操作 | macOS | Windows / Linux |
| --- | --- | --- |
| 新建 tab | Cmd+T | Ctrl+Shift+T |
| 关闭当前 tab | Cmd+W | Ctrl+Shift+W |
| 下一个 tab | Ctrl+Tab | Ctrl+Tab |
| 上一个 tab | Ctrl+Shift+Tab | Ctrl+Shift+Tab |

## 分栏

| 操作 | macOS | Windows / Linux |
| --- | --- | --- |
| 左右分栏 Split Right | Cmd+D | Ctrl+Shift+O |
| 上下分栏 Split Down | Cmd+Shift+D | Ctrl+Shift+E |

使用顶部 **Pane → Split Right** 把当前 pane 拆成左右两栏，**Split Down** 拆成上下两栏，可继续拆分任意 pane。新 pane 使用当前 Shell 设置；启动时指定的 `--shell` 或 `--ssh` 参数优先，并拥有独立会话。点击 pane 切换输入焦点，蓝色边框标记当前 pane；粘贴按钮和图片上传作用于当前 pane。

拖动分隔线调整大小。终端内容区不显示额外的目录标题和关闭按钮，目录或会话标题显示在顶部 tab 中。选择 **Pane → Close Pane** 关闭当前 pane，剩余区域自动合并。shell 退出也只关闭对应 pane；关闭最后一个 pane 时关闭该 tab。tab 的 **×** 和关闭 tab 快捷键会关闭整个 tab 的所有 pane。

## About

通过顶栏最后的 **About** 菜单项查看版本和构建提交，打开文档、GitHub 或查看随程序使用的字体及组件许可证。正式构建显示提交链接，本地构建显示开发版本标记。点击 **Close**、窗口外部或按 **Esc** 关闭。

## 从 GitHub Releases 更新

在 **About → Check for Updates** 手动检查正式版，发现新版本后可打开 **Release Notes** 查看发布说明，点击 **Download Update** 下载当前平台的完整发布包。检查、下载和校验在后台执行，不阻塞终端；网络错误或 GitHub API 限流会显示错误，可稍后重新检查。当前版本不做启动时自动检查。

下载完成后点击 **Update and Restart** 安装。此操作会关闭所有标签页和分栏、断开本地 shell 与 SSH 会话；不会恢复原会话，请先保存工作。准备完成后由独立更新进程等待主程序退出，再替换文件并以原命令行参数和工作目录重启。Windows 同时更新 `yiterm.exe`、`bin/ssh.exe`、`conpty.dll`、`OpenConsole.exe` 及随包文档和许可证。配置文件与日志不在更新包内，保持原位。

更新使用 Rust `self_update` 的 GitHub 后端、下载解压和 `MoveAll` 多文件替换，以及 `self-replace` 的更新进程清理能力；无需独立更新服务器或 GitHub Token。按 `Cargo.toml` 版本比较，只接受更高的稳定版本，并精确匹配现有 `yiterm-<版本>-<平台>` 发布包及其 `.sha256` 文件。下载后先校验 SHA-256，成功后才准备安装。该校验检查下载完整性，不是独立的发布签名验证。

程序目录必须可写，发布包中的可执行文件保持原名；目前支持 macOS ARM64、Windows x64 和 Linux x64 的便携压缩包，不支持 macOS `.app` 包更新。首次切换到带更新功能的版本仍需从 Releases 手动下载安装。若目录不可写，请先将整套程序移动到当前用户可写目录。

暂存文件位于程序目录下的 `.yiterm-update-*`，保证替换不跨文件系统。普通替换失败时尝试恢复旧文件；Windows 文件占用导致的失败会在最多 30 秒内重试，请先关闭其他 YiTerm 实例。成功后清理暂存目录；失败时保留 `error.txt` 和恢复文件，后续启动会在 About 显示安装错误。若回滚本身失败，更新进程保留备份且不自动启动可能不完整的程序，需根据错误路径恢复文件或重新解压完整发布包。多文件替换不能保证断电或强制结束更新进程时自动恢复。

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

按 UTC 天轮转，最多保留 `max_files` 个日期文件。旧版本的 `yiterm-...` 文件保留原位，新日志使用 `yiterm.` 前缀。此设置不限制单个文件大小。目录无法创建或文件无法打开时在状态栏提示；SSH wrapper 无法连接日志转发服务时会在终端显示错误。

不记录终端输出、用户输入、剪贴板正文和图片内容。Windows 内置 SSH wrapper 与主程序共用日志文件；SSH 配置解析失败时会记录具体失败项，便于排查 OpenSSH 配置。PTY、图片上传和粘贴也记录调用侧诊断信息。

### SSH 后无法选字或打开右键菜单

先按住 **Shift** 拖动选字，再按住 **Shift** 点右键。如果可用，说明普通操作被终端鼠标上报接管。排查时在 Windows PowerShell 中启动发布包内的程序：

```powershell
.\yiterm.exe --log-filter 'warn,yiterm=info,yiterm::mouse=debug'
```

也可将配置文件中的 `logging.filter` 临时设为 `warn,yiterm=info,yiterm::mouse=debug`，重启生效。macOS / Linux 可使用 `./yiterm --log-filter 'warn,yiterm=info,yiterm::mouse=debug'`。需要鼠标协议发送细节时，将最后的 `debug` 改为 `trace`。

在同一日志配置下，用两个新 pane 分别登录正常主机和有问题的主机，各执行一次普通拖动、普通右键和 Shift + 右键，再通过 **Tools → Open Log File** 查看日志：

- `Terminal session started`：`session_id` 区分 pane，`child_pid` 对应本地 shell 进程。
- `Mouse mode request`：`source=pty_output` 表示 YiTerm 收到的 PTY 指令；`mode=1000/1002/1003 enabled=true` 开启点击、拖动或所有移动上报，`enabled=false` 关闭对应模式。1006 等编码模式本身不会开启鼠标上报。
- `Terminal mode changed`：记录解析前后的模式以及最终 `mouse_reporting` 状态，可检查开启后是否恢复。
- `Terminal mouse down`：记录 Shift、选区是否存在、滚动位置及处理路径；`forward_to_application` 表示交给终端程序，`open_context_menu` 表示打开本地菜单。Linux 的右键菜单在松开按钮时触发，对应 `context_menu_on_release` 和 `Terminal context menu requested`。
- Windows 内置 SSH 的 `Mouse mode request before ConPTY`：`source=ssh_remote_output` 记录远端原始输出中的模式指令，可按 wrapper 的 PID 和时间，与主程序 PTY 侧的日志对照。使用外部 SSH 客户端时没有这一层日志。

从登录前开始记录，确认鼠标模式是在连接前已经存在，还是在远端登录、启动 tmux 或运行某个程序后开启；退出该程序后检查是否关闭。SSH 字节流不包含远端进程身份，日志不能直接指认某个 shell 或插件，需要在远端逐项禁用启动配置、插件或 tmux 后重连对照。排查结束后恢复原日志级别。

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

两种主题分别配置 ANSI 16 色；其余 256 色和程序指定的 RGB 真彩色保持原值。程序自行指定的背景颜色也会保留。浅色主题仅在单元格实际背景为主题白底时，将 ANSI 白色文字调整为深色；程序指定其他背景时，若默认前景色或 ANSI 白色/亮白色文字与实际背景的对比度低于 4.5:1，则从主题深色与纯白中选择对比度更高的颜色（例如 PowerShell 的 Ctrl+R 历史搜索区域显示为黑底白字）。程序指定的其他 ANSI 文字颜色、256 色扩展色和 RGB 真彩色文字保持原值。此规则在反显颜色交换后应用。手动编辑配置后需重启生效。

## 空闲刷新与 CPU 占用

Windows / macOS 的终端画布由 GPUI 绘制，跳过默认背景与空白字形。PTY 输出与异步粘贴完成时，通过有界通知通道唤醒界面；通知会合并，避免输出突发时重复排队。空闲时每 500 毫秒检查退出、Windows 剪贴板请求与光标闪烁，仅在内容变化或光标需要闪烁时请求重绘。后台标签页持续处理输出；窗口标题只在内容变化时更新。

Linux 保留原有 egui 的输出唤醒与每 250 毫秒兜底检查，光标按 500 毫秒边界刷新，继续合并同色背景并关闭渐变抖色。

`Linux software rendering comparison` 工作流可指定两个包含 Linux artifact 的 Actions run，在同一 CentOS 7 环境中比较空闲、持续更新的 CPU 占用与模拟按键到达 PTY 的延迟。性能数据需在相同系统、驱动与构建模式下比较；按键注入延迟不等于屏幕显示延迟。

Windows 性能验证：使用 Release 构建，启动后等待提示符稳定，在任务管理器观察空闲 CPU，并检查持续输出、后台标签页、光标闪烁、粘贴和关闭 shell 的响应。实际占用取决于设备和图形驱动，需要在 Windows 上测量。

### Windows 高 CPU 诊断

先在任务管理器展开 YiTerm 进程组，确认高占用的 PID。Process Explorer 的进程属性 → Threads 可以按 CPU 排序并查看最忙线程的 Stack。单次栈是瞬时快照，需要持续采样判断 CPU 时间消耗在会话处理还是 GPUI / DirectX 渲染。

在 PowerShell 中开启汇总诊断后启动新进程：

```powershell
$env:YITERM_PROFILE = "1"
.\yiterm.exe --log-filter "warn,yiterm=info"
```

保持终端空闲约 30 秒，然后用 **Tools → Open Log File** 打开日志，搜索 `yiterm::profile`。完成后关闭该实例，并执行 `Remove-Item Env:YITERM_PROFILE`，之后启动的实例恢复默认关闭状态。日志文件写入需保持启用（`logging.file = true`）。

Windows / macOS 启动日志记录 GPUI 窗口初始化；开启 profiling 后包含构建版本，并每约 5 秒汇总事件泵调用次数 `pumps`、画布绘制次数 `renders`、进程 CPU、事件处理与会话轮询耗时、PTY 读取次数与字节数及焦点状态。`renders_per_s` 是画布绘制频率，不等于显示器实际呈现帧率。Linux 保留原来的 OpenGL 渲染器信息、eframe 框架耗时与 egui 重绘调用位置汇总。耗时为墙钟时间，不等于线程 CPU 时间；日志不记录终端内容、按键或剪贴板正文。

`process_cpu_core_percent` 通过两次采样的进程累计 CPU 毫秒差除以墙钟时间计算，仅统计 YiTerm 当前进程，100% 表示使用一个逻辑核，不是任务管理器的进程组百分比；与系统总 CPU 百分比比较时需考虑逻辑核数量。采样依赖 UI update：若图形调用长时间卡住，日志可能延迟，此时需要外部采样。

Windows CI 额外保存 `profiling-windows-<commit>` artifact，内含同次优化构建的 `yiterm.exe` 和 `yiterm.pdb`。使用这一对文件进行诊断，不要混用其他版本的符号。通过 WPR 录制约 30 秒 CPU Usage，在 WPA 的 **CPU Usage (Sampled)** 中按 PID、线程和调用栈展开；配置该 PDB 所在目录和微软符号服务器后分析热点。可对比正常窗口/最小化、光标闪烁开/关，以及远程桌面/本机桌面。Windows 构建和采样在 Windows runner 或 Windows 设备进行，macOS 不交叉编译。

## 发布流程

版本号同步更新 `Cargo.toml` 和 `Cargo.lock`，提交后创建对应的 `v<版本号>` tag，并用 `git push --atomic origin main v<版本号>` 一起推送。

只有分支推送触发 CI，tag 推送不单独触发。一次 main 流水线完成 macOS、Windows、Linux 构建；全部构建成功后，如果当前提交有对应版本 tag，就直接使用本次构建产物发布 GitHub Release。构建的打包步骤输出当前提交的版本 tag，发布 job 检查 macOS / Windows 和 Linux 打包步骤输出的版本 tag 一致后运行；没有对应 tag 时跳过整个发布 job。若 tag 在 main 流水线结束后才推送，可在 main 上手动运行该工作流完成发布。

CI 固定使用 Rust `1.99.1`，避免浮动 `stable` 自动升级导致依赖缓存失效。升级工具链时同步修改 `.github/workflows/ci.yml` 中的 `toolchain`、Linux `RUST_TOOLCHAIN` 和 Linux 缓存键；升级后的首次构建需要重建缓存。缓存前移除 runner 预装的其他 Rust 工具链，避免缓存键受 runner 镜像更新影响。release 调试符号配置在工作流全局设置，缓存和编译使用同一配置。

## 平台兼容层与实现结构

编译目标自动选择后端，无需用户设置：

| 平台 | 界面后端 | 渲染路径 |
| --- | --- | --- |
| macOS | GPUI 0.2.2 | Metal |
| Windows | GPUI 0.2.2 | DirectX |
| Linux | 原有 egui / eframe 0.33 | OpenGL，保留旧驱动回退 |

- `src/backend.rs`：平台颜色、鼠标按钮类型与 PTY 重绘通知的适配边界。
- `src/main.rs`：共用的 PTY 会话、终端协议、标签与分栏树、SSH、链接解析和异步图片粘贴。
- `src/ui.rs`：Windows / macOS 的 GPUI 界面与输入处理。
- `src/ui_egui.rs`：Linux 原有界面、字符渲染、输入法、快捷键、鼠标与字体回退。
- `src/theme.rs` 和 `src/settings.rs`：共用主题配色和配置格式。
- `src/profiling.rs` / `src/profiling_egui.rs`：两个后端各自的性能统计，日志不记录终端内容。

Cargo 按目标平台声明依赖：Linux 不编译 GPUI；Windows / macOS 不编译 eframe、egui-winit 或 glutin。Linux 使用 `vendor/eframe`、`vendor/egui-winit` 和 `vendor/glutin` 的原有补丁；所有平台共用 `vendor/alacritty_terminal` 的 Unicode 修复。发布包按平台携带相应界面后端许可证。

界面保留 Settings / Pane / Tools / About 菜单、编号标签与 +、底部 Paste 与状态栏，以及原有设置和 About 弹窗。终端内容区直接显示终端网格，不再显示分栏标题和关闭按钮。GPUI 的 `EntityInputHandler` 接收文字与输入法预编辑 / 提交；Linux 保留 egui 的原有输入处理。

本机只运行 macOS Release 构建，不安装 Windows 工具链或交叉编译 Windows / Linux。Windows 和 Linux 的实际构建分别交给对应 CI runner。

## 字体和字号

在 **Settings → Font** 选择终端字体，使用 **Font Size** 调整字号（8–48px，默认 16px）。macOS / Windows 使用 GPUI Component 的可搜索 Select 字体选择器和 NumberInput 字号输入框（支持直接输入及加减），列出系统字体及内嵌 Lilex，隐藏系统内部字体；下方显示当前字体和字号的预览。Linux 使用相同的设置项目、说明与行布局，通过 Fontconfig 枚举已安装字体，在可搜索的下拉列表中选择；搜索框固定在弹层顶部，只有字体列表随滚轮滚动，修改搜索词后列表回到顶部。字号支持加减和直接输入。字体列表在首次打开设置时读取并缓存，新安装字体后重启程序刷新。建议选择等宽字体；平台使用可用字体回退。界面字体固定为 IBM Plex Sans Regular（13px、字重 400），独立于终端字体设置。

修改立即应用到终端标签页和分栏，自动调整字符网格与会话行列数，并保存到当前配置文件（支持 `--config`）。默认字体为内嵌 Lilex。手动编辑后重启生效：

```toml
[font]
family = "Lilex"
size = 16.0
```

在 Settings 底部点击 **Reset Defaults**，立即恢复默认主题（Dark）、终端字体（Lilex，16px）及光标（闪烁方块），恢复 Shell 为 Default，并恢复 Windows 终端链接默认在内置浏览器打开，随后自动保存。其他配置项（例如日志）保持不变。
