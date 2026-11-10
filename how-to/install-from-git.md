---
url: /tamias/how-to/install-from-git.md
---
# How to install from a git repo

Point a ref at a repo, an in-repo subdir, and a rev.

## The ref

```
git:9aia/bupha//libraries/ci-gate#main
│   │           │  │               │
│   │           │  │               └─ #ref — branch, tag, or sha
│   │           │  └─ in-repo path (after `//`)
│   │           └─ repo
│   └─ host/org (bare `org/repo` → github.com)
└─ kind
```

* `//` splits repo from the in-repo path — required because nested
  groups make `a/b/c/d` ambiguous.
* Without `//`, a two-segment prefix is the repo; the rest is the path.
* The resolved **commit sha** is recorded in the store regardless of rev.

## Install

```sh
tamias install git:9aia/bupha//libraries/ci-gate#main
tamias info ci-gate        # shows source ref + resolved sha
```

If the package ships its own `drivers/`, add `--allow-drivers` (or
`TAMIAS_ALLOW_DRIVERS=1`) — see [Write a driver](/how-to/write-a-driver).

## Track it in a project

`tamias add` does the same install *and* records the ref in the
project's `tamias.yaml` + resolved entry in `tamias.lock`:

```sh
tamias add git:9aia/bupha//libraries/ci-gate#main
```

See [Manage project dependencies](/how-to/manage-deps) for the full
add/pin/update/lock flow.

## Re-resolve a moving ref

```sh
tamias update ci-gate      # re-resolve + reinstall from the recorded ref
tamias update --all        # every installed package
tamias update --all --json # machine-readable
```

`update` and `pin` refresh `tamias.lock` when the cwd lockfile already
pins the package.

## Repoint the ref

```sh
tamias pin ci-gate git:9aia/bupha//libraries/ci-gate#main~1
```

`pin` rewrites the recorded ref; the payload is untouched until the next
`update`.

## Air-gapped / repeat installs

Git fetches are cached content-addressed under `~/.cache/tamias/fetch`,
keyed by the resolved sha — a repeat install reuses the cache and costs
nothing.

## See also

* [Manage project dependencies](/how-to/manage-deps)
* [Ref grammar](/reference/refs)
* [Tutorial: your first package](/tutorials/first-package)
