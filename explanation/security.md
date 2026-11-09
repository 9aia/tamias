---
url: /tamias/explanation/security.md
---
# Security model

Tamias moves other people's code onto your machine, so the threat model
is **supply chain**: a malicious or tampered package, a hijacked ref, a
poisoned registry. This page explains the layers; `SECURITY.md` at the
repo root is the reporting policy.

## Integrity: digests and pins

Every install records three things in the store entry: the **source
ref**, the **resolved sha** (commit sha for git, payload sha256 for
tarballs), and the **payload digest**. `tamias verify` recomputes the
digest and fails on mismatch — tampering with the store is detected.

Prefer pinned refs:

* `…#sha256:<hex>` on an http tarball — verified *before* unpack.
* git refs resolve to a commit sha up front, so the recorded `resolved`
  is immutable even when the branch moves.

Two honest caveats:

* `http://` refs **without** a `#sha256:` pin fetch cleartext and warn
  on stderr. Use `https://` or pin the digest.
* `cf:` registry installs are TLS-authenticated but **not**
  sha-pinned at transport — the registry records a payload *directory*
  digest, not the tarball sha256. For end-to-end integrity prefer `git:`
  or an http ref with `#sha256:`.

## Drivers are code — consent is explicit

A package's `type:` names a **driver** (`tamias-driver-<type>`), which
tamias executes as a subprocess. Drivers on `PATH` or in
`<tamias-home>/drivers/` are trusted host tooling.

A package that **ships its own `drivers/` dir** is different: those
binaries are arbitrary code inside the payload. Installing one requires
explicit consent:

```sh
tamias install --allow-drivers <ref>
# or once per environment:
TAMIAS_ALLOW_DRIVERS=1 tamias install <ref>
```

Without consent the install **fails before anything is registered or
run** — no partial store entry, no copied driver. The same gate exists
in the `tamias-serve` protocol as `"allow_drivers": true`.

## Missing drivers fail closed

Installing a package whose `type` has **no** discoverable driver fails
the install (for any type other than `files`) — tamias never silently
"installs" content it can't place. `tamias doctor` lists the drivers it
found so you can see what's resolvable.

## Secrets are names, never values

* Registry tokens live in env vars — `TAMIAS_REGISTRY_TOKEN` for http
  registries, or the env var *named* by a config `token_env` field.
  `tamias.yaml` / `tamias.lock` hold refs, names, and digests only.
* `tamias registry serve` is an open dev server unless you set
  `TAMIAS_REGISTRY_SERVE_TOKEN` (then every request needs
  `Authorization: Bearer …`).

## Git fetch hardening

* Clone URLs are allowlisted: `https`, `http`, `ssh`, `git`, `file`,
  and scp-style `git@host:…`. Other schemes are rejected.
* Revs are validated — no leading `-`, whitespace, `..`, or `//`, so a
  ref can't smuggle git options. Relative revs (`HEAD~2`, `main^`) are
  allowed.
* `GIT_TERMINAL_PROMPT=0` — a fetch never blocks on a credential
  prompt; it fails instead.

## Reporting

Vulnerabilities go to the maintainers via GitHub private advisories —
see `SECURITY.md`. Don't open public issues for security reports.
