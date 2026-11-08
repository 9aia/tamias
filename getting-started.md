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
tamias install git:9aia/peregrin//libraries/ci-gate#v0.2.0
tamias list
tamias info ci-gate
tamias verify ci-gate        # recompute the payload digest
```

## Author and publish

```sh
tamias bundle pack ./my-bundle
tamias publish ./my-bundle --registry local
```

## Next

* [Architecture](/explanation/architecture)
* [Content types](/explanation/content-types)
* [CLI reference](/reference/cli)
