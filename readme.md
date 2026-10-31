# YiTerm

English | [简体中文](readme.zhcn.md)

A lightweight native terminal for macOS, Windows and Linux.

![YiTerm on Windows, using the light theme with a remote session](assets/screenshots/yiterm-windows.png)

## Download and run

Download and extract the package for your platform from [Releases](https://github.com/wynemo/yiterm/releases). No Rust installation is required.

- **macOS (Apple Silicon)**: Download `macos-arm64.tar.gz` and open `YiTerm.app` (or run `./yiterm` from the extracted directory).
- **Windows 10 / 11 (x64)**: Download `windows-x64.zip` and double-click `yiterm.exe`. Keep `conpty.dll` and `OpenConsole.exe` in the same directory.
- **Linux (x64)**: Download and extract `linux-x64.tar.gz`, then run `bash ./install-desktop.sh` once to add YiTerm to your applications menu, or start it with `./yiterm`. Requires glibc 2.17 or later, X11 and a working OpenGL driver. See [Linux requirements (Chinese)](TECHNICAL.md#linux-x64--centos-78) for details.

## Features

- Multiple tabs with independent sessions.
- Horizontal and vertical split panes with draggable dividers.
- Local shells, SSH sessions, and a built-in Mosh client on Windows.
- A directory-following file browser and Git sidebar with diffs, staging and commits.
- Windows embedded WebView2 browser routed through the focused YiTerm SSH session.
- Drag to select and copy text.
- Paste text and images; images are saved as files and their paths inserted, with remote uploads supported in SSH sessions.
- Inline static images using the Kitty graphics protocol in local and SSH sessions, including the built-in SSH client on Windows.
- An embedded monospace font, with no extra font installation required.

Windows, macOS and Linux support displaying images directly in the terminal.

![YiTerm displaying an image in Codex on macOS](assets/screenshots/yiterm-macos-images.png)

![YiTerm displaying an image in Codex on Linux](assets/screenshots/yiterm-linux-images.png)

![YiTerm displaying an image in Codex over SSH on Windows](assets/screenshots/yiterm-windows-images.png)

See the [technical and usage guide (Chinese)](TECHNICAL.md) for building from source, keyboard shortcuts, configuration and logging.
