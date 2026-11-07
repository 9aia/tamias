+++
id = "ADR-0001"
title = "seed tamias from arca + re-derive peregrin's distribution engine"
status = "open"
created = "2026-11-07"
priority = "high"
+++

## Context

The factory needs a generic content/package manager. peregrin already
ships one internally (peregrin ADR-0079) — a ref grammar (`fs`/`git`/
`http`), a fetch→unpack store + content-addressed cache, and
`install/remove/list/…` verbs — but its content type is *peregrin's own*
(`main.yaml`, `.node.yml`, …) and its store is `~/.local/share/peregrin`.
myco's kinds/views/templates, kuda's domains, and mimus's rules have no
way to ship at all.

This repo is **seeded from a copy of `arca`** — its ports-and-adapters
anatomy, xtask, hooks, worktrees, `.agents/skills`, and the `-cf`
Worker + D1/R2 pattern — with the product identity renamed
`arca` → `tamias` (Greek *tamías*, "steward/treasurer"; the chipmunk
that carries, lards, scatter-caches, and recalls). The org-ledger
proposal **P-002** is the project record.

## Decision

- **Seed = arca anatomy.** Keep the workspace/xtask/hooks/worktree/
  skills scaffolding and the `-cf` crate pattern; rename identity.
- **Re-derive the crate set** (P2):
  `tamias-core` (pure) · `tamias-source{,-fs,-git,-http,-registry}` ·
  `tamias-store{,-local,-cf}` · `tamias-cache{,-local,-cf}` ·
  `tamias-content{,-peregrin,-myco,…}` ·
  `tamias-registry{,-local,-cf}` · `tamias-publish` · `tamias-cli`.
- **Lift peregrin's engine, don't rewrite.** Carry the ref grammar and
  the fetch→unpack cache split over near-verbatim from
  `peregrin-source*` + `peregrin-bundle`. peregrin ADR-0079 stays the
  record; peregrin keeps thin delegating verbs.
- **Generic engine, pluggable content types.** The manager owns bytes +
  manifest + transport + store + cache; content semantics are *declared*
  (a manifest recipe) with an optional stdio *provider* — the engine
  never links a product crate ("no plugin system: files", ADR-0082 #3).
- **Two store axes.** Registry/remote vs install store + fetch cache;
  each gets a `-local` and a `-cf` impl.
- **Provenance.** Digest-pinned refs; every install records source ref,
  resolved sha, payload digest; `verify` recomputes it.

## Status

Proposed. **P1 (seed) done** — repo created (private), identity renamed,
myco ledger + docs worktree in place, `cargo xtask check` green.
**P2 (re-derive + lift peregrin)** is next.

## Consequences

- peregrin's `install/…` verbs become thin wrappers; its
  `peregrin-bundle`/`peregrin-source` crates are deprecated in-repo.
- One manager ships peregrin bundles, myco kinds/templates, kuda domains,
  and mimus rules behind the same ref grammar — and a Cloudflare registry.
