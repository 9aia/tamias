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
# tamias-cli but its package is `tamias`).
order="tamias-core tamias-deploy tamias-deploy-cf tamias-deploy-systemd \
       tamias-driver tamias-publish tamias-registry tamias-registry-local \
       tamias-shell tamias-source tamias-source-fs tamias-source-git \
       tamias-source-http tamias-store tamias-store-cf tamias-store-local \
       tamias-telemetry tamias-testkit tamias-cache tamias-cache-cf \
       tamias-cache-local tamias-config tamias-config-toml tamias-config-yml \
       tamias-registry-cf tamias-source-cf tamias-serve tamias-cli"

touched=""
for c in $order; do
    grep -q "^crates/$c/" <<<"$changed" && touched="$touched $c"
done
# the bin re-publishes whenever any crate changed
[ -n "$touched" ] && touched="$touched tamias-cli"
if [ -z "$touched" ]; then echo "no crate changes since $last"; exit 0; fi

published=""
tags=""
for c in $order; do
    grep -qw "$c" <<<"$touched" || continue
    p="${c}"; [ "$c" = tamias-cli ] && p="tamias"   # cli package name
    cargo set-version -p "$p" --bump patch
    # crates.io rate-limits *new* crates (a time window per push); retry
    # through it rather than failing the release.
    for i in $(seq 1 60); do
        out="$(cargo publish -p "$p" --allow-dirty 2>&1)" && break
        if grep -q '429\|too many new crates' <<<"$out"; then
            echo "rate-limited on $p — sleeping 60s"; sleep 60
        elif grep -q 'already exists\|already uploaded' <<<"$out"; then
            break
        else
            echo "$out" | tail -3; sleep 30
        fi
    done
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
if command -v dist >/dev/null && command -v gh >/dev/null; then
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
