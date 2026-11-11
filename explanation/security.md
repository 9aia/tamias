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

- `…#sha256:<hex>` on an http tarball — verified *before* unpack.
- git refs resolve to a commit sha up front, so the recorded `resolved`
  is immutable even when the branch moves.
- `cf:` registry installs carry the registry index digest — pinned by
  name@version plus that digest.

Two honest caveats:

- `http://` refs **without** a `#sha256:` pin fetch cleartext and warn
  on stderr. Use `https://` or pin the digest.
- `cf:` registry installs are TLS-authenticated but **not**
  sha-pinned at transport — the registry records a payload *directory*
  digest, not the tarball sha256. For end-to-end integrity prefer `git:`
  or an http ref with `#sha256:`.

Two more warnings worth knowing:

- **`include:` transitive fetches.** A manifest `include:` that names a
  remote ref pulls *another* package at install time — and tamias warns
  when that ref is unpinned (an http(s) ref without `#sha256:`, or a
  `git:` rev that isn't a full 40-hex sha). The warning is advisory:
  remote includes are legitimate, but each one is a second supply-chain
  hop that bypasses whatever pin you put on the top-level ref.
- **Cleartext registry index.** Fetching `index.json` over `http://`
  warns on every call — the index is unauthenticated and modifiable in
  transit, so the payload digests it records are only as trustworthy as
  that GET. Signatures mitigate this *only* when
  `signing.allowed_signers` is configured (see below).

Tarball unpacking is bounded independently of `max_download` (which
caps the compressed blob): every untar enforces a **2 GiB unpacked-size
cap** and a **100k entry cap**, so a tiny `.tgz` can't expand into disk
or inode exhaustion. Manifest `type:`/`kind` strings are charset-validated
(`[a-z0-9][a-z0-9._-]*`) because the kind becomes `tamias-driver-<kind>`
in binary lookups — it can't smuggle path or option characters.

## Trust tiers: provenance, computed not declared

Signing is optional package signing (`ssh-keygen -Y`, namespace
`tamias-package`) — `publish --sign` attaches a signature, `tamias-sign`
verifies it. On top of raw signature status, every install and verify
computes a **trust tier** and records it on the store entry (`trust` +
`signer` fields):

| tier | meaning |
| --- | --- |
| `official` | signature verifies against `signing.official_signers` — the small, curated trust set |
| `signed` | signature verifies against `signing.allowed_signers`, but not the official set |
| `pinned` | no verifiable signature, but the source ref carries an integrity pin (`cf:`, `#sha256:`, a full git sha) |
| `unpinned` | no signature and no pin — a moving branch/tag, bare http(s), a local path |
| `bad` | a signature is present and verification **failed** — a failed signature is never shown as merely unsigned |

The tier is *computed* from the ref, the signature, and your signers
files — a package can't declare its own badge. Highest wins: an official
signature short-circuits before `allowed_signers` is even consulted, and
a present-but-failed signature beats any pin.

Where it surfaces:

- `tamias list` — a `trust` column.
- `tamias info` / `tamias verify` — `trust` + `signer`; `--json` carries
  the same fields, and `verify` re-derives the tier live (a recorded
  signer wins, but a rotated `allowed_signers` file is honored).
- `tamias search` / `registry get` / `tamias serve` `verify` —
  `trust`/`signer`/`publisher` fields.
- `tamias ui` — trust badges.
- `tamias doctor` — reports the resolved `official_signers` path.

### Bootstrapping `official_signers`

`allowed_signers` and `official_signers` are **two different files**:
the first says "this key may sign packages I install"; the second is the
smaller set that earns the `official` badge — think "keys we ship under"
versus "keys we merely tolerate". Resolution order for each (env wins):

| file | config | env | default |
| --- | --- | --- | --- |
| allowed | `signing.allowed_signers` | `TAMIAS_ALLOWED_SIGNERS` | `~/.config/tamias/allowed_signers` |
| official | `signing.official_signers` | `TAMIAS_OFFICIAL_SIGNERS` | `~/.config/tamias/official_signers` |

Both are `ssh-keygen -Y` `allowed_signers` files (authorized_keys
format, principals + keys). To mint an official identity:

```sh
ssh-keygen -t ed25519 -f ~/.ssh/tamias-official
# sign a package with it (key path resolution: $TAMIAS_SIGNING_KEY →
# signing.key_env → signing.key):
TAMIAS_SIGNING_KEY=~/.ssh/tamias-official tamias publish --sign ./mypkg
# authorize the *public* key for the package's signing principal —
# `@scope/name` for a scoped package, the bare `name` for unscoped:
echo 'mypkg namespaces="tamias-package" ssh-ed25519 AAAA…' \
  >> ~/.config/tamias/official_signers
```

Signing principals are **scoped**: a package `@scope/name` is signed and
verified under the principal `@scope/name`; an unscoped package falls
back to the bare `name` (and legacy records keep working the same way).
`signing.required` additionally makes unsigned installs a hard error —
but only `cf:`/registry refs can carry signatures, so `required` is a
registry-policy switch; `tamias doctor` warns when `required` is set
without a resolvable `allowed_signers`.

## Drivers are code — consent is explicit, remembered per payload

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

`TAMIAS_ALLOW_DRIVERS=1` is a **global bypass** — it covers every
install, including ones you haven't inspected; `tamias doctor` warns
when it's set. Prefer `--allow-drivers` per install, or better, check
the package first with `--plan`.

**`tamias install --plan`** is the pre-flight check: a dry run that
prints the ref, package identity, cache hit/miss, payload digest,
trust tier + signer, signature status, `include:` resolutions, bundled
drivers with their consent state, the resolved kind driver, provider
services (deploy-only), and any problems — **without executing drivers,
committing to the store, or mutating anything**. `--plan --json` gives
the same picture machine-readably. It runs `capabilities` probes on
discovered drivers (trusted local binaries), so it's safe to point at a
ref you don't trust yet.

Consent is also **remembered per package**: after a granted install,
tamias records `{label → payload digest, drivers, timestamp}` in
`~/.config/tamias/consented.json` (`$TAMIAS_CONSENT_FILE` /
`$XDG_CONFIG_HOME/tamias/` override the path). Re-installing the *same
bytes* skips re-consent; a changed payload produces a new digest and
re-prompts — consent binds to exact content, not to the package name.
The memory is shared across surfaces: the CLI flag/env, the `tamias
serve` `"allow_drivers": true` request field, and the TUI consent modal
all write the same file.

Without consent the install **fails before anything is registered or
run** — no partial store entry, no copied driver.

**Executable surfaces are a separate, ungated channel.** A package's
`bins[]` (`payload:` entries under `tools/bin/`) and legacy `bin/`
files deploy real binaries onto your `PATH` at install time with **no
consent prompt** — the `--allow-drivers` gate covers `drivers/` only.
A `shim: cli` entry is safe by construction (a generated script that
execs `tamias bin exec`), but treat any pack that declares payload
bins like you would a driver: inspect before installing
(`tamias install --plan` lists declared bins).

## Missing drivers fail closed

Installing a package whose `type` has **no** discoverable driver fails
the install (for any type other than `files`) — tamias never silently
"installs" content it can't place. `tamias doctor` lists the drivers it
found so you can see what's resolvable.

## Secrets are names, never values

- Registry tokens live in env vars — `TAMIAS_REGISTRY_TOKEN` for http
  registries, or the env var *named* by a config `token_env` field.
  `tamias.yaml` / `tamias.lock` hold refs, names, and digests only.
- `tamias registry serve` is an open dev server unless you set
  `TAMIAS_REGISTRY_SERVE_TOKEN` (then every request needs
  `Authorization: Bearer …`).

## `tamias serve` trusts its stdin

The ndjson service treats **everything on stdin as trusted**: an
`install` op can pass `"allow_drivers": true` — equivalent to
`--allow-drivers` — and there's no per-request authentication. Embed it
only behind a harness that has already made the trust decision; never
expose `tamias serve` to untrusted input.

## Git fetch hardening

- Clone URLs are allowlisted: `https`, `http`, `ssh`, `git`, `file`,
  and scp-style `git@host:…`. Other schemes are rejected.
- Revs are validated — no leading `-`, whitespace, `..`, or `//`, so a
  ref can't smuggle git options. Relative revs (`HEAD~2`, `main^`) are
  allowed.
- `GIT_TERMINAL_PROMPT=0` — a fetch never blocks on a credential
  prompt; it fails instead.

## Reporting

Vulnerabilities go to the maintainers via GitHub private advisories —
see `SECURITY.md`. Don't open public issues for security reports.
