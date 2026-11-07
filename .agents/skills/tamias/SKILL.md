---
name: tamias
description: Distribute content packages with the tamias CLI — refs (fs/git/http/cf), install/remove/list/verify, content types (peregrin bundles, myco kinds, kuda domains, mimus rules), local + Cloudflare stores, --json surfaces. Trigger when a task needs to fetch, install, publish, or verify a reusable package or content pack.
metadata:
  version: "1.0.0"
---

# tamias — content/package manager

`tamias` moves reusable **content packages** — peregrin bundles, myco
kinds/views/templates, kuda domains, mimus rules — from a source to a
machine, and keeps them verifiable. You point at a *ref*; tamias
fetches, stores, and (via a content type) installs it.

## Bootstrap

```bash
command -v tamias   # missing? install from the repo:
cargo install --git https://github.com/9aia/tamias tamias
```

## Discovery — prefer asking the CLI over reading docs

```bash
tamias manifest        # verbs, json shapes, content types (JSON)
tamias schema          # JSON Schema for tamias.yaml
tamias <verb> --help   # every verb, self-documenting
```

`--json` is honored on every verb; errors come back as `{"error": "…"}`
on stderr with a non-zero exit.

## The ref model

```
./dir                                         local (dev loop)
git:9aia/peregrin//libraries/ci-gate#v0.2.0   git + in-repo subdir + ref
https://…/ci-gate-0.2.0.tgz#sha256:<hex>        tarball (+ digest pin)
cf:<account>/<bucket>/<name>@<ver>             Cloudflare registry
```

`//subdir` splits repo from in-repo path; `#ref` is branch/tag/sha (the
version pin). A pinned digest/sha is verified on fetch.

## Verb cheat sheet — by intent

| Intent | Command |
| --- | --- |
| Install a package | `tamias install <ref>` |
| Add to the project manifest | `tamias add <ref>` |
| List / inspect | `tamias list` · `tamias info <name>` |
| Update / pin a moving ref | `tamias update <name>` · `tamias pin <name> <ref>` |
| Verify integrity | `tamias verify <name>` — recomputes the payload digest |
| Remove (cache stays) | `tamias remove <name>` |
| Author + publish | `tamias bundle pack <dir>` · `tamias publish <dir> [--registry <ref>]` |
| Serve a local registry | `tamias registry serve [--dir .]` |
| Search a registry | `tamias search <query>` |
| Housekeeping | `tamias cache clean [--all]` · `tamias doctor` |

## Content types

A package declares its `type` in its manifest (`tamias.yaml`); the type
decides *where* and *how* it installs. You usually don't pick it —
`tamias` reads it — but it's how you know what "install" means:

- `peregrin-bundle` → the peregrin bundle store (workflows, nodes, …)
- `myco-config` → merged into `myco.toml` / a seeded `project/`
- `kuda-domain` → `.kuda/domains/`
- `mimus-rules` → `rules/`
- `files` → copied to `install.into`

## The one rule for provenance

Never trust a moving ref for anything durable. Pin it (`#<sha>` or
`#sha256:<hex>`), or run `tamias verify <name>` after install. The
store record and the digest are the receipt.
