# YiTerm

English | [简体中文](readme.zhcn.md)

A lightweight native terminal for macOS, Windows and Linux.

![YiTerm on Windows, using the light theme with a remote session](assets/screenshots/yiterm-windows.png)

## Download and run

Download and extract the package for your platform from [Releases](https://github.com/wynemo/yiterm/releases). No Rust installation is required.

- **macOS (Apple Silicon)**: Download `macos-arm64.tar.gz` and run `./yiterm` from the extracted directory.
- **Windows 10 / 11 (x64)**: Download `windows-x64.zip` and double-click `yiterm.exe`. Keep `conpty.dll` and `OpenConsole.exe` in the same directory.
- **Linux (x64)**: Download `linux-x64.tar.gz` and run `./yiterm` from the extracted directory in a graphical desktop terminal. Requires glibc 2.17 or later, X11 and a working OpenGL driver. Targets CentOS 7.8 and newer Linux systems. See [Linux requirements (Chinese)](TECHNICAL.md#linux-x64--centos-78) for details.

## Features

- Multiple tabs with independent sessions.
- Horizontal and vertical split panes with draggable dividers.
- Local shells and SSH sessions.
- Drag to select and copy text.
- Paste text and images; images are saved as files and their paths inserted, with remote uploads supported in SSH sessions.
- An embedded monospace font, with no extra font installation required.

See the [technical and usage guide (Chinese)](TECHNICAL.md) for building from source, keyboard shortcuts, configuration and logging.
