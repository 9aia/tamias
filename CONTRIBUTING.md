# Contributing

Issues and PRs are **disabled** — the project is agent-maintained and
dogfoods its own PM ledger (`project/`). Fork freely under MIT.

## Layout

`crates/` — flat rust workspace, ports-and-adapters. `tamias-core` is
pure domain (no I/O); `tamias-source`/`tamias-store`/`tamias-cache`/
`tamias-content`/`tamias-registry` are ports, `-fs`/`-git`/`-http`/
`-local`/`-cf` crates their impls; `tamias` (`crates/tamias-cli`) is the
composition root. `xtask/` is repo chores.

> Seeded from `arca`; the crate set is being re-derived per org-ledger
> P-002.

## Rules of thumb

- New source / store / registry = a new impl crate, not a core change.
- New content type = a new `tamias-content-*` crate, or a declared
  manifest recipe — never a core change.
- `cargo xtask check` and `cargo xtask test` must stay green.
- Coverage: `cargo xtask coverage` — cargo-llvm-cov HTML report in
  `target/llvm-cov/html`; pass `--fail-under-lines 100` to gate.
- Keep the binary dependency-light and sync; no async runtime.
- Commits: conventional prefixes (`feat:`/`fix:`/`refactor:`/`docs:`/`chore:`).
- Digest-pin refs; secret names never values; no plaintext secret in
  tests, fixtures, docs, or logs.
- Never hand-edit `project/` when a `myco` verb exists.

## Releasing

No crates.io publish, no git tags — the install path is
`cargo install --git https://github.com/9aia/tamias tamias`.

## Docs

Follow Diátaxis: `docs/tutorials/` · `docs/how-to/` · `docs/reference/`
· `docs/explanation/`. API reference is generated from doc comments —
keep `lib.rs` module docs accurate; `docs/gen-*.py` renders
cli/config/crate-map from source.
