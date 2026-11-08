+++
id = "ADR-0002"
title = "ref + source are ports — the grammar and the transport leave core"
status = "done"
created = "2026-11-08"
priority = "high"
+++

# ref + source are ports — the grammar and the transport leave core

Status: **proposed** · Created: 2026-11-08

## Context

`tamias-core` owned the ref **grammar**: a closed `SourceRef` enum
(`Fs`/`Git`/`Http`/`Cf`) plus `parse_ref`, `GitRef`, `HttpRef`, `CfRef`.
Every consumer matched it exhaustively, and the port's `Resolved` enum was
closed too — so a new transport meant a **core change** (+ `tamias-cli` +
`tamias-serve`) *plus* an impl crate. That contradicted CONTRIBUTING's own
rule ("new source = a new impl crate, not a core change") and made core
know `git:` / `cf:` / tarball syntax.

## Decision

The grammar and the transport are **ports**; core is transport-free.

- **`RefKind`** (in `tamias-source`): claims an input and parses it into an
  opaque `Spec` (`Box<dyn Any + Send + Sync>`). `Refs` is an ordered
  registry — the first kind that claims wins; the path fallback is last.
- **`Source`** (in `tamias-source`): `accepts(&dyn Any)`,
  `plan(&dyn Any) -> Box<dyn Plan>`, `fetch(&dyn Plan, dest) -> Fetched`.
  `Sources` is the registry; `Sources::plan` returns a `Planned` (the plan
  + its owning source) so the caller never matches a variant.
- Each `tamias-source-<kind>` crate owns **both** its syntax and its
  transport: `FsRefKind`/`FsSpec`/`FsSource`, `GitRefKind`/`GitSpec`/…,
  `HttpRefKind`/…, `CfRefKind`/….
- The composition root (`tamias-cli`, `tamias-serve`) registers the kinds
  and sources — the only place the full set is known.
- `tamias-core` drops `refs.rs`; `expand_path` moves to the port.

## Consequences

- A new ref kind / transport = a new `tamias-source-<kind>` crate + one
  registry line in the composition root. **No core change.**
- `tamias-core` is pure again: package identity, manifest, lock, records,
  digests, overlay, tar — no transport syntax.
- Cost: dynamic dispatch (`Arc<dyn RefKind>` / `Box<dyn Plan>`) and a
  downcast at the spec/plan boundary.
- `Resolved` is gone as a shared enum — plans are per-source.
