#!/usr/bin/env bash
# Register the extracted portable package for the current desktop user.
set -euo pipefail

if [[ $(uname -s) != Linux ]]; then
  echo 'Run this installer on Linux.' >&2
  exit 1
fi

directory=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
if [[ $data_home != /* ]]; then
  echo 'XDG_DATA_HOME must be an absolute path.' >&2
  exit 1
fi
if [[ ! -f $directory/yiterm || ! -f $directory/yiterm.png ]]; then
  echo 'Keep install-desktop.sh beside yiterm and yiterm.png in the extracted package.' >&2
  exit 1
fi
# Desktop entry values must stay on one line; Exec paths cannot contain '='.
case "$directory" in
  *$'\n'*|*$'\r'*|*=*)
    echo 'Move the package to a path without newlines, carriage returns or =.' >&2
    exit 1
    ;;
esac

escape_value() {
  local value=$1
  value=${value//\\/\\\\}
  value=${value//$'\t'/\\t}
  printf '%s' "$value"
}

escape_exec() {
  local value=$1
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  value=${value//\$/\\\$}
  value=${value//\`/\\\`}
  value=${value//%/%%}
  escape_value "$value"
}

chmod u+x -- "$directory/yiterm"
applications="$data_home/applications"
mkdir -p -- "$applications"
temporary=$(mktemp "$applications/.yiterm-XXXXXX")
trap 'rm -f -- "$temporary"' EXIT
cat > "$temporary" <<EOF
[Desktop Entry]
Type=Application
Name=YiTerm
Comment=Native terminal emulator
Exec="$(escape_exec "$directory/yiterm")"
Icon=$(escape_value "$directory/yiterm.png")
Terminal=false
Categories=System;TerminalEmulator;
StartupWMClass=yiterm
EOF
chmod 644 -- "$temporary"
mv -f -- "$temporary" "$applications/yiterm.desktop"
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$applications" || true
fi
printf 'YiTerm is now available in the applications menu.\nLauncher: %s\nKeep the package at: %s\n' \
  "$applications/yiterm.desktop" "$directory"
