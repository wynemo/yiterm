# YiTerm

轻量原生终端，支持 macOS 和 Windows。

## 下载运行

从 [Releases](https://github.com/wynemo/yiterm/releases) 下载对应平台的压缩包并解压，无需安装 Rust。

- **macOS（Apple Silicon）**：下载 `macos-arm64.tar.gz`，在解压目录中运行 `./yiterm`。
- **Windows（x64）**：下载 `windows-x64.zip`，双击 `yiterm.exe`。请将 `conpty.dll` 和 `OpenConsole.exe` 保留在同一目录。

## 功能

- 多标签页，每个标签页运行独立会话。
- 左右、上下分栏，可拖动调整大小。
- 本地 shell 和 SSH 远端会话。
- 拖动选择文字并复制。
- 粘贴文字和图片；图片保存为文件并输入路径，SSH 会话支持上传到远端。
- 内置等宽字体，无需额外安装。

构建、快捷键、配置和日志等详情见 [技术与详细使用说明](TECHNICAL.md)。
