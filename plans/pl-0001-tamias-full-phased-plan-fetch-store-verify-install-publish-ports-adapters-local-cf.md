+++
id = "PL-001"
title = "tamias — full phased plan (fetch/store/verify/install/publish; ports+adapters; local+cf)"
status = "open"
created = "2026-11-07"
owner = "human:luis"
priority = "high"
epic = "tamias"
milestone = "v1"
+++

## Goal

One generic **package & content manager** for the 9aia factory: fetch,
store, verify, install, remove, publish, and search reusable content
(peregrin bundles, myco configs, kuda domains, mimus rules) behind one
ref grammar and one identity model. Ships as the `tamias` CLI. The
engine is generic; content is pluggable.

## Locked decisions

- **Name:** tamias (chipmunk; Gk. *tamías*, "steward/treasurer").
- **Seed:** a copy of `arca`, renamed. Model: **ports + adapters**
  (`local` and `cf` are just adapters).
- **Version:** semver (the `semver` crate).
- **Identity:** `@scope/name@version` — namespaces first-class.
- **Lockfile:** `tamias.lock` (YAML).
- **Publishing:** in scope.
- **peregrin cutover:** no legacy compatibility — tamias *becomes* the
  official distribution.
- **Provenance:** sha256 pin + `verify` (v1); SSH signing (v2).

## The model (ports → adapters)

| Port | Job | Adapters |
| --- | --- | --- |
| `source` | turn a ref into bytes | `-fs` · `-git` · `-http` · `-registry` |
| `store` | keep installed packages | `-local` · `-cf` |
| `cache` | keep downloads (reuse/offline) | `-local` · `-cf` |
| `registry` | publish + index | `-local` · `-cf` |
| `content` | decide *where/how* a kind installs | `-peregrin` · `-myco` · `-kuda` · `-mimus` · `-files` |

Adding a backend = a new adapter crate; no core change.

## Crate map (final)

```
tamias-core      identity(@scope/name@semver), Ref, Manifest, records, digest (pure)
tamias-source    port (+ -fs, -git, -http, -registry)
tamias-store     port (+ -local, -cf)
tamias-cache     port (+ -local, -cf)
tamias-registry  port (+ -local, -cf)
tamias-content   port (+ -peregrin, -myco, -kuda, -mimus, -files)
tamias-publish   author side: pack + push
tamias-testkit   conformance for every adapter
tamias-cli       the `tamias` binary
xtask            check / test / coverage / install / site
```

## Identity & namespaces

- `tamias.yaml` declares deps; `tamias.lock` pins resolved identity + sha
  + digest (YAML).
- `@scope/name` in semver; store path `store/@scope/name@ver/`; registry
  key `@scope/name@ver`.
- Scope defaults from the source org. **Scoped packages resolve only
  from their own registry — no public fallback** (kills dependency
  confusion).

## Security baseline (v1)

- sha256 pin + `verify`; lockfile; immutable versions; scopes-with-no-
  fallback; declarative install only (no code runs on install); publish
  tokens via **arca** (names, never values).
- **v2:** SSH signing + `allowed-signers`. **Later:** transparency log,
  sigstore, SBOM.

## Phases

### P0 — Ledger & alignment
Org proposal **P-002** + this plan; repo **ADR-0001**. *Gate:* committed.

### P1 — Seed (DONE)
Copy `arca` → `tamias`; rename; docs; `myco init`; private repo; three
branches; `cargo xtask check` green.

### P2 — Engine core
Goal: working `install/list/info/verify/remove` against local fixtures.
1. **Prune** the seed: drop `tamias-vault*`, `-identity*`, `-deploy*`
   (secret domain).
2. **Add** `tamias-core`, `tamias-source{,-fs,-git,-http}`,
   `tamias-store{,-local}`, `tamias-cache{,-local}`, `tamias-cli`.
3. **Lift** peregrin's `source` + `bundle` (ref grammar, manifest,
   store, cache, overlay, archive, digest).
4. **Identity**: `@scope/name@semver` (the `semver` crate); store layout;
   install records.
5. **Ref grammar**: fs / git / http (+ a `registry` stub).
6. **Lockfile**: read/write `tamias.lock` (YAML).
7. **CLI**: `install/add/remove/list/info/update/pin/verify/cache clean`
   with `--json` + `manifest`/`schema`.
8. **Tests**: fixtures + adapter conformance.
*Gate:* `cargo xtask check/test` green; `tamias install ./fixture` then
`verify` passes; lockfile stable across re-runs.

### P3 — Content port + peregrin type
`tamias-content` (trait + declared-type registry) and
`tamias-content-peregrin` reproducing today's install semantics; params
schema validation.
*Gate:* installing a peregrin bundle yields the same store as peregrin's
manager today.

### P4 — peregrin cutover (no compat)
Repoint peregrin's `install/…` onto `tamias-*`; delete `peregrin-source`
+ `peregrin-bundle` + `distribution.rs`. No shims.
*Gate:* peregrin `check/test` green; `peregrin install` works through
`tamias`.

### P5 — Local registry + publish
`tamias-registry` + `-local` (dir index), `tamias-publish` (pack +
push), `search`, `registry serve`.
*Gate:* pack → publish → install-from-registry roundtrip; lockfile pins
the result.

### P6 — Cloudflare adapters
`tamias-registry-cf` (Worker + R2 + D1/KV), `tamias-store-cf`,
`tamias-cache-cf`; content-addressed R2 keys; **immutable versions**;
publish audit log; token via **arca**.
*Gate:* private push/pull with an arca-supplied token; a re-publish of an
existing version is refused.

### P7 — Second content type (myco)
`tamias-content-myco` (kinds/views/templates) + a `pm` pack.
*Gate:* `tamias add @9aia/myco-pm` merges kinds into `myco.toml`.

### P8 — Dogfood + docs
Publish 9aia's bundles (ci-gate, release) + the `pm` pack to Cloudflare;
docs site; `.agents/skills`.
*Gate:* docs site builds; a real bundle is published + installed via `cf:`.

## Cross-cutting

- Every phase: `cargo xtask check/test` green; docs + ledger updated;
  conventional-prefix commits.
- Branches: `main` (code) · `project` (ledger) · `docs` (site) — the
  worktree model, as in the siblings.

## Risks & mitigations

- **Pruning arca code** while keeping green → do it crate-by-crate, gate
  each step.
- **peregrin cutover** → accepted (no-compat decision); keep it a
  mechanical repoint.
- **Cloudflare auth** → arca-backed tokens; scope-limited.
- **Scoped-name store format** → brand new; cheap to change now.

## Definition of done (v1)

`tamias install git:…` + `verify` + lockfile; peregrin uses tamias as its
only distribution; a local registry publishes + installs; a private
Cloudflare registry works with an arca token; the `myco` pm pack installs.
