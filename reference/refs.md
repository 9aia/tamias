# Ref grammar

A **ref** names *where* a package comes from. One grammar covers all
sources; the impl is picked by shape.

```
./dir                                         local (dev loop)
git:9aia/peregrin//libraries/ci-gate#v0.2.0   git + in-repo subdir + ref
https://…/ci-gate-0.2.0.tgz#sha256:<hex>        tarball (+ digest pin)
cf:<account>/<bucket>/<name>@<ver>             Cloudflare registry
```

## Git

- `//subdir` splits the repo from the in-repo path (go-getter
  convention) — required because nested groups make bare `a/b/c/d`
  ambiguous. Without `//`, a two-segment prefix is the repo and the
  remainder is the in-repo path.
- `#ref` is a branch, tag, or sha — the version pin. The resolved
  commit sha is recorded in the store regardless.
- `org/repo` defaults to `github.com`; `host/org/repo` becomes
  `https://host/org/repo`; `git@host:…` and `ssh://`/`file://` pass
  through.

## HTTP

- A `.tgz` / `.tar.gz` URL, with an optional `#sha256:<hex>` pin.

## Cloudflare registry

```
cf:<account>/<bucket…>/<name>@<version>
```

The address derives the base URL: a full `http(s)://…` passes through
(local / dev registries), a first segment with **no dot** is a
`workers.dev` subdomain, and later segments are a path prefix.

```
cf:9aia/myrepo/ci-gate@0.2.0        → https://9aia.workers.dev/myrepo
cf:reg.example.com/ci-gate@0.2.0    → https://reg.example.com
cf:http://127.0.0.1:8903/w@1.0.0    → http://127.0.0.1:8903
```

`install` fetches `<base>/index.json`, finds `name@version`, then pulls
the tarball (`<base>/<file>`) over HTTP.

## Local

- `./x`, `../x`, `~/x`, `/x`, or a bare path.
