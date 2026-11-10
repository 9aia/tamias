+++
id = "PL-006"
title = "tamias sync — phased delivery"
status = "open"
created = "2026-11-10"
owner = "devin"
priority = "medium"
links = ["P-003"]
+++

Proposal: P-003. Pattern precedent: T-014 extractions — shared logic
lives in tamias-core/tamias-install/tamias-registry-open, surfaces
delegate.

## Phase 1 — core sync op

- [ ] Shared sync function (home: tamias-install or tamias-core — pick
      the crate whose deps allow Manifest+Lock+Store+install; likely a
      small `tamias-sync`-style fn inside tamias-install since it already
      deps store-local? check — else tamias-core can't do I/O; decide at
      impl, document).
- [ ] Semantics per proposal: per-dep install-if-missing-or-drifted,
      lock refresh, per-dep status, aggregate result; never removes;
      `--locked` opt-in if cheap.
- [ ] Progress surface: reuse `install_with_progress` + Surface so CLI
      lines and TUI log strip work unchanged.

## Phase 2 — surfaces

- [ ] `tamias sync [--json] [--locked]` CLI verb (clap + handler +
      manifest/gen-cli-docs regen).
- [ ] `tamias-serve` op `sync` (same Env shape; cwd manifest discovery
      same rule as CLI — document that serve syncs the *server's* cwd).
- [ ] TUI Manifest pane `s` key → sync job via ops.rs worker machinery.

## Phase 3 — tests + docs

- [ ] Tests: temp project manifest+lock, fake/local package sources
      (reuse existing test fixtures — fs: refs to temp dirs), missing/
      drifted/current dep arms, failure aggregation, --json shape,
      serve op dispatch, PTY smoke if cheap.
- [ ] Docs: docs/ how-to (extend manage-deps.md — it already documents
      add/update/pin; add sync section), regenerate cli.md via
      gen-cli-docs, config/manifest docs if schema changed (it won't).
- [ ] `cargo xtask check` + `test` green; `--no-default-features` clean;
      `kuda sync --full`.
