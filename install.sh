#!/bin/sh
# Installs sey by building it from source with cargo.
# curl -fsSL https://raw.githubusercontent.com/iantabolt/sey/master/install.sh | sh
set -eu

REPO="https://github.com/iantabolt/sey"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo is required but not found." >&2
    echo "install Rust first: https://rustup.rs" >&2
    exit 1
fi

echo "Installing sey from $REPO..."
cargo install --git "$REPO" --locked sey

echo "Done. Run 'sey --help' to get started."
