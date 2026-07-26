# Every recipe runs cargo inside `nix-shell`, which is what provides the system
# libraries the build needs (Vulkan, Wayland/X11, fontconfig, freetype, dbus).
#
# `shell.nix` describes a Linux desktop, and nix-shell is not something a
# Windows machine has at all, so `_cargo` falls back to plain cargo when
# nix-shell is unavailable. macOS and Windows contributors get the same recipe
# names; they are responsible for having a toolchain in scope.
_cargo := if `command -v nix-shell >/dev/null 2>&1 && echo yes || echo no` == "yes" { "nix-shell --quiet --run" } else { "sh -c" }

default: run

run:
    {{_cargo}} "cargo run"

release:
    {{_cargo}} "cargo run --release"

check:
    {{_cargo}} "cargo check --all-targets"

test:
    {{_cargo}} "cargo test"

build:
    {{_cargo}} "cargo build --release"

fmt:
    {{_cargo}} "cargo fmt"

clippy:
    {{_cargo}} "cargo clippy --all-targets -- -D warnings"

clean:
    cargo clean

shell:
    nix-shell

# Assembles Aviary.app around the release binary (macOS only): a bundle
# identifier is what allows notifications, and CFBundleURLTypes is what
# registers the `mailto:` handler.
bundle-macos: build
    packaging/macos/bundle.sh

logout:
    rm -f ${XDG_CONFIG_HOME:-$HOME/.config}/aviary/pending_tokens.json ${XDG_CONFIG_HOME:-$HOME/.config}/aviary/accounts/*.json
