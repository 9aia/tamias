# tamias

*Package & content manager for the agent factory — fetch, store,
cache, dispense.*

Named for *Tamias* — the chipmunk; Greek *tamías* = "steward /
treasurer", after its food-hoarding. Cheek pouches carry a payload
home, a larder holds it, scatter-caches hide it by key, and it recalls
each stash. tamias is the content steward: it distributes, installs,
and removes **content packages** — and it doesn't care what the content
*is*.

```
source ─► tamias install <ref> ─► fetch ─► content-addressed cache
               │                                │
               ▼                                ▼
        content-type adapter ──────────► install store <name>@<ver>
        (peregrin bundle, myco kinds, …)
```

## Why

peregrin already ships a package manager internally (ADR-0079) — a ref
grammar (`fs`/`git`/`http`), a fetch→unpack store + cache, and
`install/remove/list/…` verbs — but its content type is *peregrin's own*
and its store is `~/.local/share/peregrin`. myco's kinds/views/templates,
kuda's domains, and mimus's rules have no way to ship at all. tamias is
the generic manager they share.

## What it moves

A **package** is *bytes + a manifest*, wherever it lives. tamias owns
transport, store, and cache; **content types** own the meaning:

| Content type | Payload | Installs into |
| --- | --- | --- |
| `peregrin-bundle` | workflows, `.node.yml`, `.snippet.yml`, `.field.yml`, `.prompt.yml` | the peregrin bundle store |
| `myco-config` | `[[kinds]]`, `[views]`, `[verbs]`, templates | `myco.toml` / a seeded `project/` |
| `kuda-domain` | `data/domains/*.toml` | `.kuda/domains/` |
| `mimus-rules` | `rules/*.toml` | `rules/` |
| `files` | anything | `install.into` (escape hatch) |

## Ref grammar

```
./dir                                         local (dev loop)
git:9aia/peregrin//libraries/ci-gate#v0.2.0   git + in-repo subdir + ref
https://…/ci-gate-0.2.0.tgz#sha256:<hex>        tarball (+ digest pin)
cf:<account>/<bucket>/<name>@<ver>             Cloudflare registry (P6)
```

## Verbs

| Verb | What it does |
| --- | --- |
| `tamias install <ref>` | resolve → fetch → store (content-type install) |
| `tamias add <ref>` | record in `tamias.yaml` + install |
| `tamias remove <name>` | drop the store entry (cache untouched) |
| `tamias list` / `info` | installed packages, refs, digests |
| `tamias update` / `pin` | re-resolve a moving ref / retarget the ref |
| `tamias verify <name>` | digest-check the store vs the record |
| `tamias cache clean` | evict the fetch cache (age policy / `--all`) |
| `tamias search <query>` | query a (local \| Cloudflare) registry |
| `tamias publish <dir>` | push a package to a registry |
| `tamias registry serve` | serve a local registry (dev / air-gap) |
| `tamias bundle pack` | dir + manifest → `.tgz` |
| `tamias doctor` | store / cache / registry / content-type diagnostics |
| `tamias config [--file f] [--json]` | show the effective config (YAML standard, TOML alt) |

`--json` on `doctor`; `tamias manifest` / `tamias schema` self-discover.

## Stores — local and Cloudflare

- **install store** — `<name>@<version>/` under `~/.local/share/tamias`,
  each with a record (source ref, resolved sha, digest, installed-at).
- **fetch cache** — content-addressed `~/.cache/tamias/fetch/<key>/`;
  repeats and air-gapped nodes cost nothing.
- **registry** — where published content lives: a directory
  (`tamias registry serve`) or a Cloudflare Worker + R2 (payloads) +
  D1/KV (index). Same protocol either way.

## Layout

Ports-and-adapters, flat `crates/` workspace. `tamias-core` is pure
domain (no I/O); ports are traits, impls are separate crates;
`crates/tamias-cli` (package `tamias`) is the composition root.

| Crate | Role |
| --- | --- |
| `tamias-core` | package, ref grammar, manifest, records, digest — pure |
| `tamias-source` + `-fs,-git,-http,-cf` | source port (`resolve`→`fetch`) + impls |
| `tamias-store` + `-local,-cf` | install store port + impls |
| `tamias-cache` + `-local,-cf` | content-addressed fetch cache |
| `tamias-driver` | discoverable content drivers (`tamias-driver-<type>`, JSON over stdio) |
| `tamias-deploy` + `-local,-cf` | the service / deploy port + impls |
| `tamias-registry` + `-local,-cf` | index + publish port + impls |
| `tamias-publish` | author-side orchestration |
| `tamias-config` + `-yml,-toml` | config model + file formats (YAML standard, TOML alt) |
| `tamias-testkit` | impl conformance |
| `tamias-cli` | the `tamias` binary |
| `xtask` | check / test / coverage / install / site |

## Position in the stack

tamias = **content distribution**. Neighbors: **peregrin** (orchestration
— its `install/…` verbs become thin wrappers here), **myco** (work memory
— kinds/views/templates ship as a content type), **kuda** (code memory —
domain packs), **mimus** (rules packs), **cuora** (secrets — publish tokens
via `cuora://`), **sturn** (claims the store lock), **bupha**/**ci-gate**
(the first published bundle), **sepia** (sessions), **cicada** (scheduled
updates).

## Status

Working v0.1 — `install` · `add` · `remove` · `list [--type]` · `info` ·
`verify` · `update` · `pin` · `cache` · `publish` · `search` · `bundle
pack` · `registry serve` · `deploy-registry` · `deploy` · `uninstall` ·
`service` · `doctor` · `schema` · `gen-cli-docs` · `serve` over fs/git/http
sources, a content-addressed fetch cache, a local **and Cloudflare**
store / registry / deploy, a discoverable **driver** system
(`tamias-driver-<type>`, JSON over stdio) with the peregrin provider
cutover complete. Remaining: P10c product drivers (T-010).

## Install

```sh
cargo install --git https://github.com/9aia/tamias tamias
cargo xtask install        # from a checkout → ~/.cargo/bin
```

## Docs

`docs/` worktree — VitePress + Diátaxis → `9aia.com/tamias`;
`cargo xtask site` builds it. Repo docs: [AGENTS](AGENTS.md) ·
[CONTRIBUTING](CONTRIBUTING.md) · [SECURITY](SECURITY.md) ·
[LICENSE](LICENSE) (MIT) · [project](project/index.md).
