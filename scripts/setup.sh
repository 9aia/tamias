#!/usr/bin/env bash
# Fresh-checkout setup: point git at the repo's hooks and sanity-check
# the toolchain. Run once after cloning (idempotent).
set -euo pipefail
cd "$(dirname "$0")/.."

git config core.hooksPath .hooks
hooks="$(git config core.hooksPath)"
if [ "$hooks" != ".hooks" ]; then
    echo "setup: core.hooksPath is '$hooks', expected '.hooks'" >&2
    exit 1
fi
for h in .hooks/pre-commit .hooks/pre-push; do
    [ -x "$h" ] || { echo "setup: $h missing or not executable" >&2; exit 1; }
done
echo "setup: core.hooksPath=.hooks (pre-commit: cargo fmt --check, pre-push: cargo xtask check)"

# rust-toolchain.toml pins channel + components; rustup applies it here.
rustc --version
cargo --version
cargo fmt --version >/dev/null && cargo clippy --version >/dev/null \
    || { echo "setup: rustfmt/clippy missing — 'rustup component add rustfmt clippy'" >&2; exit 1; }
echo "setup: ok"
