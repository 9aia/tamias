# How to install from a git repo

Point a ref at a repo, an in-repo subdir, and a rev.

## The ref

```
git:9aia/peregrin//libraries/ci-gate#v0.2.0
│   │           │  │               │
│   │           │  │               └─ #ref — branch, tag, or sha
│   │           │  └─ in-repo path (after `//`)
│   │           └─ repo
│   └─ host/org (bare `org/repo` → github.com)
└─ kind
```

- `//` splits repo from the in-repo path — required because nested
  groups make `a/b/c/d` ambiguous.
- Without `//`, a two-segment prefix is the repo; the rest is the path.
- The resolved **commit sha** is recorded in the store regardless of rev.

## Install

```sh
tamias install git:9aia/peregrin//libraries/ci-gate#v0.2.0
tamias info ci-gate        # shows source ref + resolved sha
```

## Re-resolve a moving ref

```sh
tamias update ci-gate      # re-resolve + reinstall from the recorded ref
tamias update --all        # every installed package
```

## Repoint the ref

```sh
tamias pin ci-gate git:9aia/peregrin//libraries/ci-gate#v0.3.0
```

`pin` rewrites the recorded ref; the payload is untouched until the next
`update`.

## Air-gapped / repeat installs

Git fetches are cached content-addressed under `~/.cache/tamias/fetch`,
keyed by the resolved sha — a repeat install reuses the cache and costs
nothing.

## See also

- [Ref grammar](/reference/refs)
- [Tutorial: your first package](/tutorials/first-package)
