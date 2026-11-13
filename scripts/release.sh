#!/usr/bin/env bash
# tamias release — thin shim over tarsi-serve (the org release plane,
# project ADR-0005; repo config: ./tarsi.toml). One call runs
# plan → changelog → bump → tag → push → publish.
#
# tamias is the ONE repo that publishes to crates.io (ADR-0004) —
# `[publish.crates] enabled = true` in tarsi.toml, `CARGO_REGISTRY_TOKEN`
# resolved by name at publish time, 429 backoff on. tarsi topo-sorts
# publish order from the workspace dep graph (the old hand-maintained
# `order=` list is gone). `tamias-xtask` is skipped; `tamias-testkit`
# publishes last like before. GitHub Releases / binary dist stay off.
# Operator-run on main — `tarsi-serve release` is dry-run by
# default; --yes is the explicit opt-in.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
# worktree-local secrets (gitignored); the service env file is canonical
[ -f ./.env ] && { set -a; source ./.env; set +a; }

tarsi-serve release --dir . --yes

# post-publish smoke: install the published CLI and run it. Catches a
# stale already-published dependency (in-workspace builds hide this
# class of bug). On failure, yank and republish the stale crate.
if ! cargo xtask smoke; then
    echo "SMOKE FAILED — the published CLI does not build/run. Yank and republish the stale crate." >&2
    exit 1
fi

echo "released: see tags tamias-*-v<ver> (package tamias → tag tamias-v<ver>)"
