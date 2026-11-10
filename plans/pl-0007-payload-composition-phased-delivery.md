+++
id = "PL-007"
title = "payload composition — phased delivery"
status = "open"
created = "2026-11-10"
owner = "devin"
priority = "high"
links = ["P-004"]
+++

Proposal: P-004. Order: include machinery first (install+publish share
the resolver), lock-only second (independent, smaller).

## Phase 1 — include resolver + install composition

- [ ] `include:` resolution in tamias-install post-manifest/pre-driver:
      store-name → payload dir; ref → recursive install (depth cap,
      cycle detection); missing name → hard error.
- [ ] overlay() compose into scratch; driver dispatch + store commit +
      digest on COMPOSED tree; manifest keeps include: for provenance.
- [ ] Progress events for include resolution (fetching base X, composing).
- [ ] Tests: nested includes, ordering (earlier wins, local wins),
      yaml/json doc-merge vs whole-file, cycle/depth errors, missing
      base, digest stability, verify-after-install.

## Phase 2 — publish/pack flatten

- [ ] publish/bundle pack: default flatten includes via same resolver
      (read-only — never installs); `--keep-includes` ships raw.
- [ ] Unresolvable base → fail naming it. Manifest unchanged.
- [ ] Tests: pack flatten produces composed tgz; keep-includes raw;
      missing base error.

## Phase 3 — lock-only install

- [ ] `tamias sync --from-lock` (or best-fit verb): iterate tamias.lock
      entries → install recorded source_ref; per-entry report; no
      manifest consult, no lock rewrite.
- [ ] Serve op + TUI `:sync --from-lock` if trivial; tests.

## Phase 4 — polish

- [ ] docs/ update (explanation/content-types or architecture +
      manage-deps/how-to), gen-cli-docs regen, AGENTS.md if semantics
      documented there; kuda sync --full; xtask check/test green.
