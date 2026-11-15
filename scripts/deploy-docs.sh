#!/usr/bin/env bash
# Build the VitePress site and publish .vitepress/dist to the `gh-pages`
# branch. Runs identically by hand (from the docs/ worktree) and under
# the CI gate — the peregrin ci-gate campaign (bupha is retired) runs
# `bash scripts/deploy-docs.sh` in a detached worktree of the pushed
# docs sha, so cwd is the docs checkout root and
# `..` is NOT the main repo. Cargo work happens in TAMIAS_REPO (the real
# checkout); the gen-api.py rustdoc pipeline honors TAMIAS_ROOT for the
# same reason. cli/config/crate-map pages come from `cypera gen`
# driven by $REPO/cypera.toml.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
DOCS="$(pwd)"
REPO="${TAMIAS_REPO:-$HOME/GitHub/9aia/tamias}"
export TAMIAS_ROOT="$REPO"

echo "==> bun install"
bun install

CYPERA="cargo run -q --manifest-path $REPO/../cypera/Cargo.toml -p cypera --"
echo "==> reference pages (cypera gen, repo=$REPO)"
$CYPERA gen cli --repo "$REPO"
$CYPERA gen config --repo "$REPO"
$CYPERA gen crate-map --repo "$REPO"

echo "==> rustdoc → public/api (cargo doc in $REPO)"
python3 gen-api.py

echo "==> vitepress build"
bunx vitepress build

# Branch deploys still get a Jekyll pass unless .nojekyll is present.
touch .vitepress/dist/.nojekyll

echo "==> deploy .vitepress/dist → gh-pages"
bunx gh-pages -d .vitepress/dist -b gh-pages \
  -r git@github.com:9aia/tamias.git \
  -m "docs: deploy $(git rev-parse --short HEAD)"

echo "done — https://9aia.com/tamias/ serves origin/gh-pages"
