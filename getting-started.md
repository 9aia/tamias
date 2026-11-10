---
url: /tamias/getting-started.md
---
# Getting Started

Install tamias, then point it at a package.

## Install

```sh
cargo install --git https://github.com/9aia/tamias tamias
# or, from a checkout:
cargo xtask install
```

## Install a package

```sh
tamias install git:9aia/bupha//libraries/ci-gate#main
tamias list
tamias info ci-gate
tamias verify ci-gate        # recompute the payload digest
tamias doctor                # store, cache, git, registry, drivers — all green?
```

## Author and publish

```sh
tamias bundle pack ./my-bundle
tamias publish ./my-bundle            # writes to the default local registry
# or a served registry:
# tamias publish ./my-bundle --registry http://127.0.0.1:8788
```

## Next

* [Architecture](/explanation/architecture)
* [Content types](/explanation/content-types)
* [Security model](/explanation/security) — digests, drivers, `--allow-drivers`
* [Manage project dependencies](/how-to/manage-deps)
* [CLI reference](/reference/cli)
