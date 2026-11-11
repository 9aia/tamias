#!/usr/bin/env bash
# Manual release — run by an operator on main (bupha watch, the old
# push trigger, is retired; releases are operator-run).
# The leading sleep is the debounce window: only the last invocation
# in a quiet window releases. Commits/tags are pushed to main.
#
# Publishes to crates.io in dependency order (a crate's path deps must
# already be on the registry). Needs: `cargo-edit` (cargo set-version)
# and, for binary artifacts, `cargo-dist` (dist) + `gh`.
set -euo pipefail
# worktree-local secrets (gitignored); the service env file is canonical
[ -f ./.env ] && { set -a; source ./.env; set +a; }
sleep "${TAMIAS_RELEASE_DEBOUNCE:-300}"

git fetch origin --tags -q
last="$(git describe --tags --abbrev=0 2>/dev/null || true)"
if [ -z "$last" ]; then
    echo "no tags yet — run 'tamias deploy release' to seed the flow"; exit 0
fi
changed="$(git diff --name-only "${last}..HEAD")"

# Publish order — dependencies first (dir names; the cli dir is
# tamias-cli but its package is `tamias`). Keep in sync with the
# workspace dep graph: every crate's tamias-* deps must precede it.
order="tamias-core tamias-registry tamias-config tamias-registry-local \
       tamias-registry-cf tamias-source tamias-source-http tamias-source-cf \
       tamias-config-toml tamias-config-yml tamias-store-local \
       tamias-source-fs tamias-publish tamias-registry-open \
       tamias-source-registry tamias-source-git tamias-cache-local \
       tamias-sign tamias-driver tamias-install tamias-shell tamias-deploy \
       tamias-deploy-launchd tamias-deploy-windows tamias-deploy-systemd \
       tamias-provider tamias-tui tamias-deploy-cf tamias-serve \
       tamias-telemetry tamias-cli tamias-cache tamias-cache-cf \
       tamias-store tamias-store-cf tamias-testkit"

if [ -z "$changed" ]; then echo "no changes since $last"; exit 0; fi

# Versions are workspace-inherited — a bump is all-or-nothing, so every
# publishable crate republishes each release (dep reqs all move
# together). `cargo set-version --workspace` bumps once; per-crate
# `-p` bumps cascade the workspace version and corrupt dep reqs.
cargo set-version --workspace --bump patch

published=""
tags=""
for c in $order; do
    p="${c}"; [ "$c" = tamias-cli ] && p="tamias"   # cli package name
    # crates.io rate-limits *new* crates (a time window per push); retry
    # through it rather than failing the release. Other failures retry
    # briefly too (index lag) but bail fast on hard errors.
    ok=""
    for i in $(seq 1 60); do
        out="$(cargo publish -p "$p" --allow-dirty 2>&1)" && { ok=1; break; }
        if grep -q '429\|too many new crates' <<<"$out"; then
            echo "rate-limited on $p — sleeping 60s"; sleep 60
        elif grep -q 'already exists\|already uploaded' <<<"$out"; then
            ok=1; break
        elif [ "$i" -ge 3 ]; then
            echo "$out" | tail -5; echo "publish failed on $p" >&2; exit 1
        else
            sleep 15
        fi
    done
    [ -n "$ok" ] || { echo "publish failed on $p (rate limit window)" >&2; exit 1; }
    published="$published $p"
    # Per-crate tag — versions diverge independently in a monorepo.
    ver_c="$(grep -m1 '^version' "crates/$c/Cargo.toml" | cut -d'"' -f2)"
    tags="$tags $p-v$ver_c"
done

git commit -aqm "release: bump crate versions" || true
for t in $tags; do git tag "$t"; done
ver="$(grep -m1 '^version' crates/tamias-cli/Cargo.toml | cut -d'"' -f2)"
git push -q origin HEAD:main --tags

# binaries: local dist build (host arch) → GitHub Release.
# Opt-in: set TAMIAS_RELEASE_DIST=1 — dist/gh artifacts are not part of
# every release.
if [ "${TAMIAS_RELEASE_DIST:-0}" = "1" ] && command -v dist >/dev/null && command -v gh >/dev/null; then
    dist build --tag="tamias-v${ver}" --output-format=json > /tmp/dist-manifest.json 2>&1 || true
    gh release create "tamias-v${ver}" --generate-notes \
        --title "tamias ${ver}" 2>/dev/null || true
    for f in target/distrib/*; do
        [ -f "$f" ] && gh release upload "tamias-v${ver}" "$f" --clobber
    done
fi
# registry-only smoke: install the published CLI and run it. Catches a stale
# already-published dependency (in-workspace builds hide this class of bug).
if ! cargo xtask smoke; then
    echo "SMOKE FAILED — the published CLI does not build/run. Yank and republish the stale crate." >&2
    exit 1
fi

echo "released:${published} (tags:${tags})"
