# YiTerm

轻量原生终端，支持 macOS、Windows 和 Linux。

![YiTerm 在 Windows 上运行，使用浅色主题连接远端会话](assets/screenshots/yiterm-windows.png)

## 下载运行

从 [Releases](https://github.com/wynemo/yiterm/releases) 下载对应平台的压缩包并解压，无需安装 Rust。

- **macOS（Apple Silicon）**：下载 `macos-arm64.tar.gz`，在解压目录中运行 `./yiterm`。
- **Windows（x64）**：下载 `windows-x64.zip`，双击 `yiterm.exe`。请将 `conpty.dll` 和 `OpenConsole.exe` 保留在同一目录。
- **Linux（x64）**：下载 `linux-x64.tar.gz`，在图形桌面的终端中进入解压目录，运行 `./yiterm`。最低要求 glibc 2.17，目标支持 CentOS 7.8 及更新的 Linux 系统；需要 X11 和可用的 OpenGL 驱动。详细要求见 [Linux 使用说明](TECHNICAL.md#linux-x64--centos-78)。

## 功能

- 多标签页，每个标签页运行独立会话。
- 左右、上下分栏，可拖动调整大小。
- 本地 shell 和 SSH 远端会话。
- 拖动选择文字并复制。
- 粘贴文字和图片；图片保存为文件并输入路径，SSH 会话支持上传到远端。
- 内置等宽字体，无需额外安装。

构建、快捷键、配置和日志等详情见 [技术与详细使用说明](TECHNICAL.md)。
