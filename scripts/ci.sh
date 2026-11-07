#!/usr/bin/env bash
# bupha gate for main — runs in a detached worktree of the pushed sha.
# check = fmt + clippy -D warnings; test = the whole suite.
set -euo pipefail

cargo xtask check
cargo xtask test
