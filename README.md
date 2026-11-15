# tamias

*Package & content manager for the agent factory — fetch, store,
cache, dispense.*

Named for *Tamias* — the chipmunk; Greek *tamías* = "steward /
treasurer", after its food-hoarding. tamias is the content steward: it
distributes, installs, and removes **content packages**, and it
doesn't care what the content *is*. Content meaning lives in pluggable
*content types* — one manager ships peregrin bundles, myco
kinds/templates, kuda domains, mimus rules, and anything else.

```
source ─► tamias install <ref> ─► fetch ─► content-addressed cache
               │                                │
               ▼                                ▼
        content-type adapter ──────────► install store <name>@<ver>
```

## Quickstart

```sh
cargo install tamias        # crates.io
tamias install git:9aia/myco//libraries/myco@v0.1.0
tamias list                 # installed packages
tamias ui                   # interactive TUI (optional)
```

## Why

Every 9aia product needs to ship content — workflows, config kinds,
rules, packs — and each was growing its own package manager. tamias is
the generic one: ref grammar (`fs`/`git`/`http`/`cf:`/named-registry),
transport, a content-addressed fetch cache, an install store, and
publish/search against a local or Cloudflare registry — plus an
optional content-type driver protocol and declared `bins[]` for
product CLIs.

## Docs

- [Docs site](https://9aia.com/tamias) — tutorials, how-tos,
  reference, architecture (VitePress + Diátaxis, in the `docs/`
  worktree)
- [AGENTS.md](AGENTS.md) — crate map, commands, rules, boundary
- [CONTRIBUTING.md](CONTRIBUTING.md) · [SECURITY.md](SECURITY.md)
- [LICENSE](LICENSE) (MIT) · [project ledger](project/index.md)
