+++
id = "PL-007"
title = "payload composition — phased delivery"
status = "claimed"
created = "2026-11-10"
owner = "devin"
priority = "high"
links = ["P-004"]
+++

Proposal: P-004. Order: include machinery first (install+publish share
the resolver), lock-only second (independent, smaller).

## Phase 1 — include resolver + install composition

- [x] `include:` resolution in tamias-install post-manifest/pre-driver:
      store-name → payload dir; ref → recursive install (depth cap,
      cycle detection); missing name → hard error.
- [x] overlay() compose into scratch; driver dispatch + store commit +
      digest on COMPOSED tree; manifest keeps include: for provenance.
- [x] Progress events for include resolution (fetching base X, composing).
- [x] Tests: nested includes, ordering (earlier wins, local wins),
      yaml/json doc-merge vs whole-file, cycle/depth errors, missing
      base, digest stability, verify-after-install.

## Phase 2 — publish/pack flatten

- [x] publish/bundle pack: default flatten includes via same resolver
      (read-only — never installs); `--keep-includes` ships raw.
- [x] Unresolvable base → fail naming it. Manifest unchanged.
- [x] Tests: pack flatten produces composed tgz; keep-includes raw;
      missing base error.

## Phase 3 — lock-only install

- [x] `tamias sync --from-lock` (or best-fit verb): iterate tamias.lock
      entries → install recorded source_ref; per-entry report; no
      manifest consult, no lock rewrite.
- [x] Serve op + TUI `:sync --from-lock` if trivial; tests.

## Phase 4 — polish

- [x] docs/ update (explanation/content-types or architecture +
      manage-deps/how-to), gen-cli-docs regen, AGENTS.md if semantics
      documented there; kuda sync --full; xtask check/test green.
