#!/usr/bin/env bash
# Triggered by bupha watch on pushes to 9aia/tamias (script runner).
# The leading sleep is the debounce window: a newer push kills this
# run via cancel_running, so only the last push in a quiet window
# releases. Runs inside a detached worktree — commits/tags are pushed
# from here, not from the main checkout.
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
order="tamias-core tamias-deploy tamias-deploy-cf tamias-deploy-local \
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
    for i in 1 2 3 4 5; do
        cargo publish -p "$p" --allow-dirty && break || sleep 30
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
    dist build --output-format=json > /tmp/dist-manifest.json 2>&1 || true
    gh release create "tamias-v${ver}" --generate-notes \
        --title "tamias ${ver}" 2>/dev/null || true
    for f in target/distrib/*; do
        [ -f "$f" ] && gh release upload "tamias-v${ver}" "$f" --clobber
    done
fi
echo "released:${published} (tags:${tags})"
