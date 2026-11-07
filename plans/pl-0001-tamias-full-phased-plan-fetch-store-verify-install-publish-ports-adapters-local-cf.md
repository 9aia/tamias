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
- **Version:** semver (the `semver` crate). **v1 = exact pins**
  (semver exact / git sha / sha256 digest); ranges resolve against the
  registry index → later.
- **Identity:** `@scope/name@version` — namespaces first-class.
- **Manifests:** a *package* manifest and a *project* manifest are both
  `tamias.yaml` (same name, different dirs — like `Cargo.toml`). Project
  deps pin into **`tamias.lock` (YAML)**.
- **Publishing:** in scope.
- **peregrin cutover:** no legacy compatibility — tamias *becomes* the
  official distribution.
- **Provenance:** sha256 pin + `verify` (v1); SSH signing (v2).

## Ports (precise roles)

| Port | Role | Adapters |
| --- | --- | --- |
| `source` | turn a ref into bytes | `-fs` · `-git` · `-http` · `-registry` |
| `store` | keep package **content** (content-addressed blobs) | `-local` (a dir) · `-cf` (R2) |
| `cache` | local read-through copy of fetched content | `-local` (a shared `-cf` later) |
| `registry` | **index** + publish: maps `@scope/name@ver` → a store location | `-local` (dir index) · `-cf` (Worker + D1/KV) |
| `content` | materialize a package into its **install target** | `-peregrin` · `-myco` · `-kuda` · `-mimus` · `-files` |

The **install target** is chosen by the content type (`install.into`),
default `~/.local/share/tamias/store`. Adding a backend = a new adapter
crate; no core change.

## Install path

```
ref → resolve → (cache hit?) → fetch → read package manifest →
      content type → materialize into target → record (ref, sha, digest)
```

## Crate map (final)

```
tamias-core      identity(@scope/name@semver), Ref, Manifest, records, digest (pure)
tamias-source    port (+ -fs, -git, -http, -registry)
tamias-store     port (+ -local, -cf)
tamias-cache     port (+ -local)
tamias-registry  port (+ -local, -cf)
tamias-content   port (+ -peregrin, -myco, -kuda, -mimus, -files)
tamias-publish   author side: pack + push
tamias-testkit   conformance for every adapter
tamias-cli       the `tamias` binary
xtask            check / test / coverage / install / site
```

No dependency cycle: tamias is standalone; **peregrin depends on tamias**,
never the reverse. Drop arca's `tui` at v1; an ndjson `tamias serve` is a
later adapter (peregrin shells out to the `tamias` CLI at v1).

## Lift set (corrected — verified against peregrin)

`peregrin-source`, `peregrin-source-{fs,git,http}`, `peregrin-bundle`,
**plus `peregrin-git`** (the git-CLI helpers `source-git` needs) and the
**`deep_merge` overlay helper** from `peregrin-loader-yaml` (which
`bundle`'s `include:` overlay uses).

## Security baseline (v1)

- sha256 pin + `verify`; lockfile; immutable versions; scopes-with-no-
  fallback; declarative install only (no code runs on install); publish
  tokens via **arca** (names, never values).
- **v2:** SSH signing + `allowed-signers` (signature stored beside the
  digest in the manifest) + a preflight policy. **Later:** transparency
  log, sigstore, SBOM.

## Phases

### P0 — Ledger & alignment
Org proposal **P-002** + this plan; repo **ADR-0001**. *Gate:* committed.

### P1 — Seed (DONE)
Copy `arca` → `tamias`; rename; docs; `myco init`; private repo; three
branches; `cargo xtask check` green.

### P2a — Skeleton
1. **Prune** the seed: drop `tamias-vault*`, `-identity*`, `-deploy*`
   (secret domain) and `tamias-tui`.
2. **Add** empty crates: `core`, `source{,-fs,-git,-http}`,
   `store{,-local}`, `cache{,-local}`, `cli` (wired into the workspace).
*Gate:* `cargo xtask check/test` green (empty crates; nothing regressed).

### P2b — Engine core
1. **Lift** the set above into the new crates; strip peregrin specifics.
2. **Identity** `@scope/name@semver` (the `semver` crate) + store layout
   + install records.
3. **Ref grammar**: fs / git / http (+ a `registry` stub).
4. **Manifest + lockfile**: package `tamias.yaml` (name/version/type/
   install) and project `tamias.yaml` (deps) + `tamias.lock` (YAML).
5. **CLI**: `install/add/remove/list/info/update/pin/verify/cache clean/
   doctor` with `--json` + `manifest`/`schema`.
6. **Tests**: fixtures + adapter conformance.
*Gate:* `cargo xtask check/test` green; `tamias install ./fixture` then
`verify` passes; lockfile stable across re-runs.

### P3 — Content port + peregrin type
`tamias-content` (trait + declared-type registry) and
`tamias-content-peregrin` reproducing today's install semantics; params
schema validation.
*Gate:* installing a peregrin bundle yields the same store as today.

### P4 — peregrin cutover (no compat)
Repoint peregrin's `install/…` onto `tamias-*` (shell out to the CLI at
v1); delete `peregrin-source` + `peregrin-bundle` + `distribution.rs`. No
shims.
*Gate:* peregrin `check/test` green; `peregrin install` works through
`tamias`.

### P5 — Local registry + publish
`tamias-registry` + `-local` (dir index), `tamias-publish` (pack +
push), `search`, `registry serve`. Define the **registry protocol**
(`GET /index`, `GET /@scope/name/ver`, `PUT`; auth; immutability).
*Gate:* pack → publish → install-from-registry roundtrip; lockfile pins
it; a version can't be overwritten.

### P6 — Cloudflare adapters
`tamias-registry-cf` (Worker + R2 + D1/KV), `tamias-store-cf` (R2),
`tamias-cache-cf`; content-addressed R2 keys; **immutable versions**;
publish audit log; token via **arca**.
*Gate:* private push/pull with an arca-supplied token; a re-publish of an
existing version is refused.

### P7 — Second content type (myco)
`tamias-content-myco` (kinds/views/templates) + a `pm` pack (a *provider*
merge into `myco.toml`, or a declared recipe).
*Gate:* `tamias add @9aia/myco-pm` merges kinds into `myco.toml`.

### P8 — Dogfood + docs
Publish 9aia's bundles (ci-gate, release) + the `pm` pack to Cloudflare;
docs site; `.agents/skills`.
*Gate:* docs site builds; a real bundle is published + installed via `cf:`.

## Milestones

- **M1 (P2–P3):** a working local engine + the first content type.
- **M2 (P4–P5):** peregrin uses tamias; local registry publish/install.
- **M3 (P6–P8):** Cloudflare registry + myco pack + dogfood.

## Cross-cutting

Every phase: `cargo xtask check/test` green; docs + ledger updated;
conventional-prefix commits. Branches: `main` (code) · `project`
(ledger) · `docs` (site).

## Open decisions

- Package manifest filename (keep `tamias.yaml` vs `tamias-package.yaml`).
- Content types: data-only vs shipping a provider binary (start data-only;
  providers only where a product must act).
- Cloudflare auth: static token vs arca-only.
- `registry:` refs — a distinct `source` or sugar over `http`.
- Semver ranges: exact-only at v1, ranges with index resolution later.

## Risks & mitigations

- **Pruning arca code** while keeping green → crate-by-crate, gate each
  step (P2a).
- **peregrin cutover** → accepted (no-compat); a mechanical repoint.
- **Cloudflare auth** → arca-backed, scope-limited.
- **Scoped-name store format** → brand new; cheap to change now.

## Definition of done (v1)

`tamias install git:…` + `verify` + lockfile; peregrin uses tamias as its
only distribution; a local registry publishes + installs; a private
Cloudflare registry works with an arca token; the `myco` pm pack installs.
