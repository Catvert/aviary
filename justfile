_nix := "nix-shell --quiet --run"

default: run

run:
    {{_nix}} "cargo run"

release:
    {{_nix}} "cargo run --release"

check:
    {{_nix}} "cargo check --all-targets"

test:
    {{_nix}} "cargo test"

build:
    {{_nix}} "cargo build --release"

fmt:
    {{_nix}} "cargo fmt"

clippy:
    {{_nix}} "cargo clippy --all-targets -- -D warnings"

clean:
    cargo clean

shell:
    nix-shell

logout:
    rm -f ${XDG_CONFIG_HOME:-$HOME/.config}/aviary/pending_tokens.json ${XDG_CONFIG_HOME:-$HOME/.config}/aviary/accounts/*.json
