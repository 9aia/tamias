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

peregrin already ships a package manager internally (its own distribution ADR) — a ref
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
| `files` | anything | the store entry (no driver — the escape hatch) |

## Ref grammar

```
./dir                                         local (dev loop)
git:9aia/peregrin//libraries/ci-gate#v0.2.0   git + in-repo subdir + ref
https://…/ci-gate-0.2.0.tgz#sha256:<hex>        tarball (+ digest pin)
cf:<account>/<bucket>/<name>@<ver>             Cloudflare registry (P6)
9aia:<name>@<ver>                              named registry (config)
```

## Verbs

| Verb | What it does |
| --- | --- |
| `tamias install <ref>` | resolve → fetch → store (content-type install); deploys declared `bins[]` unless `--no-bins` |
| `tamias add <ref>` | record in `tamias.yaml` + install |
| `tamias sync` | reconcile the project's `deps:`/`tamias.lock` against the store (`--locked`, `--from-lock`) |
| `tamias remove <name>` | drop the store entry (cache untouched) |
| `tamias uninstall <name>` | provider teardown + units + drop the store entry |
| `tamias list` / `info` | installed packages, refs, digests (`--type`, `--json`) |
| `tamias update` / `pin` | re-resolve a moving ref / retarget the ref |
| `tamias verify <name>` | digest-check the store vs the record |
| `tamias bin list/deploy/destroy` | manage declared `bins[]` + `bin/` payload bins on the bin dir |
| `tamias self-upgrade` | update the tamias-managed binary you're running |
| `tamias deploy` / `service` / `run` / `logs` | provider services — per-user units (systemd/launchd/Task Scheduler), foreground exec, journals |
| `tamias driver exec` / `drivers` | dispatch provider ops verbs / list discovered drivers |
| `tamias provider list` | installed packages' `provider:` blocks + unit state |
| `tamias cache clean` | evict the fetch cache (age policy / `--all`) |
| `tamias search <query>` | query a (local \| Cloudflare) registry |
| `tamias publish <dir>` | push a package to a registry (`--sign`, `--keep`) |
| `tamias registry serve` | serve a local registry (dev / air-gap) |
| `tamias bundle pack` | dir + manifest → `.tgz` |
| `tamias ui` | interactive TUI (ratatui) |
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
| `tamias-core` | package identity, manifest (`bins[]`, `provider:`), lock, records, digest — pure, transport-free |
| `tamias-source` + `-fs,-git,-http,-cf,-registry` | ref-grammar + source ports (`RefKind`/`Source`) + per-kind impls (`-registry` = named-registry refs) |
| `tamias-store` + `-local,-cf` | install store port + impls |
| `tamias-cache` + `-local,-cf` | content-addressed fetch cache |
| `tamias-registry` + `-local,-cf` + `-open` | index + publish port + impls + shared name-or-ref resolution |
| `tamias-publish` | author-side packing |
| `tamias-install` | the one install pipeline — ref → plan → fetch/cache → manifest → driver dispatch → store commit |
| `tamias-bin` | declared `bins[]` lifecycle — shims, `tools/bin/<triple>/` payloads, bin dir deploy/destroy/list |
| `tamias-driver` | discoverable content drivers (`tamias-driver-<type>`, JSON over stdio) |
| `tamias-driver-tamias` | pavo view driver over the tamias store + the engine ops surface (`cli.<verb>` manifest, `run`/`cli` passthroughs) |
| `tamias-provider` | provider lifecycle — `provider:` blocks → units, deploy/uninstall, `run`/`logs` |
| `tamias-deploy` + `-systemd,-launchd,-windows,-cf` | deploy port — per-user services (picked by `cfg!`) + remote registry provisioning |
| `tamias-sign` | optional package signing (`ssh-keygen -Y`, sturn-style) |
| `tamias-config` + `-yml,-toml` | config model + file formats (YAML standard, TOML alt) |
| `tamias-serve` | ndjson JSON-RPC over stdio — the embedding surface |
| `tamias-tui` | `tamias ui` — ratatui package-manager TUI |
| `tamias-shell` / `tamias-telemetry` | exec helper / opt-in OTLP layer |
| `tamias-testkit` | fixtures + impl conformance (scaffold — empty, reserved) |
| `tamias` (`tamias-cli`) | the engine lib (`command()`/`run()`) + the `tamias` compat bin — stays native: every `bins[]` shim execs `tamias bin exec` |
| `xtask` | check / test / coverage / install / package / smoke / site |

## Position in the stack

tamias = **content distribution**. Neighbors: **peregrin** (orchestration
— its `install/…` verbs become thin wrappers here), **myco** (work memory
— kinds/views/templates ship as a content type), **kuda** (code memory —
domain packs), **mimus** (rules packs), **cuora** (secrets — publish tokens
via `cuora://`), **sturn** (claims the store lock), **bupha** — the
CI repo (`type: bupha` bundles; the org gate is mid-migration back
from the peregrin `ci-gate` campaign), **sepia** (sessions), **cicada** (scheduled
updates).

## Status

Working v0.1 — `install` · `add` · `sync` · `remove` · `uninstall` ·
`list [--type]` · `info` · `verify` · `update` · `pin` · `self-upgrade`
· `bin list/deploy/destroy` · `cache` · `publish` (`--sign`, `--keep`
retention) · `search` · `bundle pack` · `registry serve|add|remove` ·
`deploy-registry` · `deploy` · `service` · `run` · `logs` · `driver
exec` · `provider list` · `ui` · `doctor` · `schema` · `gen-cli-docs` ·
`serve` over fs/git/http/named-registry sources, a content-addressed
fetch cache, a local **and Cloudflare** store / registry / deploy, a
discoverable **driver** system (`tamias-driver-<type>`, JSON over stdio)
with the peregrin provider cutover complete, declared `bins[]` packs
(product CLIs ship as `shim: cli` + `tools/bin/<triple>/` payloads),
optional ssh package signing, and a pavo ops surface on
`tamias-driver-tamias`. Remaining: P10c product drivers (T-010).

## Install

```sh
cargo install tamias        # crates.io
cargo install --git https://github.com/9aia/tamias tamias   # from source
cargo xtask install        # from a checkout → ~/.cargo/bin
```

`tamias` itself stays a native cargo binary — every `bins[]` shim in
the org execs `tamias bin exec`, so it can't retire into a shim. The
product CLIs (`myco`, `cuora`, `kuda`, `sepia`, `lasius`, `bupha`,
`cicada`, `peregrin`) are **not** cargo-installable: they ship as tamias
packs — `tamias install 9aia:<name>@<version>`.

## Docs

`docs/` worktree — VitePress + Diátaxis → `9aia.com/tamias`;
`cargo xtask site` builds it. Repo docs: [AGENTS](AGENTS.md) ·
[CONTRIBUTING](CONTRIBUTING.md) · [SECURITY](SECURITY.md) ·
[LICENSE](LICENSE) (MIT) · [project](project/index.md).
