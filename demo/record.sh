#!/bin/sh
# Regenerates the README demo GIFs with vhs (https://github.com/charmbracelet/vhs).
#
# The demos run against rust-lang/regex's `regex-lite` crate, pinned to a
# fixed commit, so recordings are reproducible without vendoring that
# source into this repo.
set -eu

cd "$(dirname "$0")/.."
DEMO_DIR="$(pwd)/demo"
REGEX_COMMIT="2b527599eb9eea0dcc288c704584f242f26a5c61"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "Building sey..."
cargo build --release

echo "Fetching rust-lang/regex@${REGEX_COMMIT:0:12}..."
git clone --quiet https://github.com/rust-lang/regex.git "$WORK/regex-src"
git -C "$WORK/regex-src" checkout --quiet "$REGEX_COMMIT"

export PATH="$(pwd)/target/release:$PATH"
export SEY_DEMO_SRC="$WORK/regex-src/regex-lite/src"
export SEY_DEMO_SCRATCH="$WORK/scratch"

for tape in "$DEMO_DIR"/*.tape; do
    rm -rf "$SEY_DEMO_SCRATCH"
    cp -r "$SEY_DEMO_SRC" "$SEY_DEMO_SCRATCH"
    echo "Recording $(basename "$tape")..."
    vhs "$tape"
done

echo "Done. GIFs written to $DEMO_DIR."
