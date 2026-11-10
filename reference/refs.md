# Ref grammar

A **ref** names *where* a package comes from. One grammar covers all
sources; the impl is picked by shape.

```
./dir                                         local (dev loop)
git:9aia/bupha//libraries/ci-gate#main   git + in-repo subdir + ref
https://…/ci-gate-0.2.0.tgz#sha256:<hex>        tarball (+ digest pin)
cf:<account>/<bucket>/<name>@<ver>             Cloudflare registry
9aia:<name>@<ver>                              named registry (config)
9aia:@scope/<name>@<ver>                        named registry, scoped
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

## Named registry

```
<label>:<name>@<version>
<label>:@scope/<name>@<version>
```

`<label>` resolves against the *effective* config's `registries` map —
the user-level `~/.config/tamias/tamias.*` base overlaid by the cwd
`tamias.*` (same layering as `publish`/`search`). An entry's `id`
(uuid or slug-safe string, `registry add --id`) matches **before** the
map key, so refs can pin a stable id across renames. Only `http`/`cf`
kind registries are installable — a `local` (unserved dir) registry
errors with a pointer at `tamias registry serve`. An unknown label is
a hard error (`unknown registry '<label>'`), never a silent path
fallback.

```
tamias install 9aia:cicada-demo@0.1.0
tamias install 018f3c2e-…:@acme/scoped@2.0.0
```

`@<version>` is required — there is no "latest" resolution. Fetching
goes through the same path as `cf:` (index lookup → payload-digest +
signature verification → tarball fetch), and the store record keeps
the verbatim `<label>:…` ref as provenance. Bearer auth uses the
registry's `token_env` (env var *name*, falling back to
`TAMIAS_REGISTRY_TOKEN`). A `<label>:` that matches a reserved kind
prefix (`git:`, `cf:`, `http(s):`, `ssh:`, `fs:`, `file:`) is never a
registry ref, and single-letter labels are excluded so Windows drive
letters (`C:\…`) can't false-positive.

The same refs work in a project manifest's `deps:` — `tamias sync`
reconciles them like any other ref.

## Local

- `./x`, `../x`, `~/x`, `/x`, or a bare path.
