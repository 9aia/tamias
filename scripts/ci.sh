#!/usr/bin/env bash
# bupha gate for main — runs in a detached worktree of the pushed sha.
# check = fmt + clippy -D warnings; test = the whole suite.
set -euo pipefail

cargo xtask check
cargo xtask test

# advisory scan — best-effort; skipped when the tool is absent
command -v cargo-audit >/dev/null && cargo audit || echo 'cargo-audit not installed — skipping'
