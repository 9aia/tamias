# Ref grammar

A **ref** names *where* a package comes from. One grammar covers all
sources; the impl is picked by shape.

```
./dir                                         local (dev loop)
git:9aia/bupha//libraries/ci-gate#main   git + in-repo subdir + ref
https://…/ci-gate-0.2.0.tgz#sha256:<hex>        tarball (+ digest pin)
cf:<account>/<bucket>/<name>@<ver>             Cloudflare registry
```

## Git

- `//subdir` splits the repo from the in-repo path (go-getter
  convention) — required because nested groups make bare `a/b/c/d`
  ambiguous. Without `//`, a two-segment prefix is the repo and the
  remainder is the in-repo path.
- `#ref` is a branch, tag, sha, or **relative rev** (`HEAD~2`, `main^`)
  — the version pin. The resolved commit sha is recorded in the store
  regardless. Revs can't start with `-` or contain whitespace, `..`, or
  `//`, so a ref can't smuggle git options.
- Clone URLs are allowlisted: `https`, `http`, `ssh`, `git`, `file`,
  and scp-style `git@host:…`. Anything else is rejected.
- Fetches run with `GIT_TERMINAL_PROMPT=0` — a credential prompt fails
  the fetch instead of hanging.
- `org/repo` defaults to `github.com`; `host/org/repo` becomes
  `https://host/org/repo`; `git@host:…` and `ssh://`/`file://` pass
  through.

## HTTP

- A `.tgz` / `.tar.gz` URL over `http(s)`, with an optional
  `#sha256:<hex>` pin — verified against the downloaded bytes before
  unpack. An `http://` URL **without** a pin fetches cleartext and
  prints a warning; prefer `https://` or pin the digest.

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

`install` fetches `<base>/index.json`, finds `name@version`, pulls the
tarball (`<base>/<file>`), unpacks it, and **verifies the unpacked
payload against the index's recorded `dir_digest`** — a mismatched
payload fails the install. Note the digest covers the payload *dir*, not
the tarball bytes, so it can't be checked at transport (see
[Security model](/explanation/security)).

## Local

- `./x`, `../x`, `~/x`, `/x`, or a bare path.
