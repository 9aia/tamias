# Contributing

Issues and PRs are **disabled** — the project is agent-maintained and
dogfoods its own PM ledger (`project/`). Fork freely under MIT.

## Layout

`crates/` — flat rust workspace, ports-and-adapters. `tamias-core` is
pure domain (no I/O); `tamias-source`/`tamias-store`/`tamias-cache`/
`tamias-content`/`tamias-registry` are ports, `-fs`/`-git`/`-http`/
`-local`/`-cf` crates their impls; `tamias` (`crates/tamias-cli`) is the
composition root. `xtask/` is repo chores.

> Seeded from `cuora`; the crate set is being re-derived per org-ledger
> the org-ledger seed proposal.

## Rules of thumb

- New **transport** (source/store/registry) = a new impl crate, not a core
  change. New **ref kind** = a new `tamias-source-<kind>` crate (grammar +
  transport) + one line in the composition root — also not a core change
  ([ADR-0002](project/adrs/0002-ref-source-are-ports-the-grammar-and-the-transport-leave-core.md)).
- New content type = a new `tamias-content-*` crate, or a declared
  manifest recipe — never a core change.
- `cargo xtask check` and `cargo xtask test` must stay green.
- Coverage: `cargo xtask coverage` — cargo-llvm-cov HTML report in
  `target/llvm-cov/html`. The gate is `cargo xtask coverage
  --fail-under-lines 97`, run under `RUSTC_BOOTSTRAP=1` so test code is
  excluded (`cfg_attr(coverage, coverage(off))`). llvm-cov folds region
  coverage into its line metric (LLVM PR #203740), so a small artifact
  tail keeps it just under 100.
- Keep the binary dependency-light and sync; no async runtime.
- Commits: conventional prefixes (`feat:`/`fix:`/`refactor:`/`docs:`/`chore:`).
- Digest-pin refs; secret names never values; no plaintext secret in
  tests, fixtures, docs, or logs.
- Never hand-edit `project/` when a `myco` verb exists.

## Releasing

Run `cargo xtask package` first — it packages + verifies every crate as
crates.io will see it (catches files that escape a crate).

After publishing, run `cargo xtask smoke` — it installs `tamias` from crates.io
and runs it. This is the only check that catches a **stale already-published
dependency** (in-workspace builds hide it, and `cargo package --workspace`
resolves workspace members by path, not by registry version).

Never edit a crate that is already published: bump it and republish, or the
registry copy silently drifts from your source.

Crates publish to **crates.io** in dependency order via `scripts/release.sh`
(bupha-triggered on push). The binary is also installable from source:
`cargo install --git https://github.com/9aia/tamias tamias`. The script needs
`cargo-edit` (`cargo set-version`) and, for artifacts, `cargo-dist` (`dist`) + `gh`.

## Docs

Follow Diátaxis: `docs/tutorials/` · `docs/how-to/` · `docs/reference/`
· `docs/explanation/`. API reference is generated from doc comments —
keep `lib.rs` module docs accurate; `docs/gen-*.py` renders
cli/config/crate-map from source.
