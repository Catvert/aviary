#!/bin/sh
# Installs Aviary for the current user under ~/.local — no root, no package
# manager. Distribution packages should install the same three files into
# /usr/bin and /usr/share instead of calling this.
set -eu

prefix="${PREFIX:-$HOME/.local}"
here="$(cd "$(dirname "$0")" && pwd)"

install -Dm755 "$here/aviary"          "$prefix/bin/aviary"
install -Dm644 "$here/aviary.desktop"  "$prefix/share/applications/aviary.desktop"
install -Dm644 "$here/aviary.svg"      "$prefix/share/icons/hicolor/scalable/apps/aviary.svg"

# Launchers cache .desktop files; a fresh entry stays invisible until the
# database is rebuilt. Not fatal when the tool is missing.
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$prefix/share/applications" 2>/dev/null || true
fi

printf 'Aviary installed to %s\n' "$prefix/bin/aviary"

case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) printf 'Note: %s is not on your PATH.\n' "$prefix/bin" ;;
esac
