+++
id = "ADR-0003"
title = "binary distribution — release tarballs ride existing https refs; no new channel in v1"
status = "done"
created = "2026-11-10"
priority = "medium"
+++

# binary distribution — release tarballs ride existing https refs; no new channel in v1

Status: **accepted**

## Context

Tamias moves *content* payloads — manifests, workflow libraries,
provider specs. It has no channel for **compiled artifacts**: the only
binary path today is the `drivers/bin/<triple>/<name>` payload
convention inside a package (compiled driver binaries ride the tgz like
any other file). PL-008 / P-008 needs tamias to eventually distribute
*itself* and provider binaries (`hypha`, `hypha-hub`, `tamias`,
compiled drivers) to hosts.

What already exists:

- **`https:` tarball refs with `#sha256:` pins** — `tamias-source-http`
  GETs `https://….tgz|.tar.gz`, verifies the pin, caches by
  `sha256-<hex>`. Integrity is enforced today, zero new code.
- **cargo-dist is already configured** in `dist-workspace.toml` across
  the org's Rust repos (tamias, hypha, myco, peregrin, sepia, sturn,
  kuda) — GitHub-release artifacts are the org's binary pipeline.
- **crates.io** is the source-of-truth channel (`cargo install`,
  `cargo xtask package`/`smoke` in tamias).
- **`cf:` registry** (`tamias-registry-cf`) is scaffolded — a hosted
  index + Worker could carry artifacts later.
- **Package signing shipped** (P-005/I-003): `tamias-sign` verifies
  ssh-sign signatures on registry (`cf:`) installs and `tamias verify`,
  with computed trust tiers (`official`/`signed`/`pinned`/`unpinned`/
  `bad`) and a separate `official_signers` root. https-tarball pins
  remain integrity-only — the authenticity gap this ADR flags is
  narrower than when proposed but still open for the https channel.

## Decision

**v1: no new ref scheme, no new source kind.** Binaries are ordinary
tamias packages whose payload is an unpacked release tree, fetched via
the existing `https://<release-asset>.tar.gz#sha256:<hex>` ref. The
sha256 pin *is* the distribution pin — the same mechanism content
packages already use.

Concretely:

1. **Artifact channel = GitHub release tarballs** produced by
   cargo-dist. Each repo publishes `https://github.com/9aia/<repo>/
   releases/download/v<ver>/<bin>-<triple>.tar.gz`; consumers install
   `tamias install "https://…tar.gz#sha256:<digest>"`.
2. **Payload layout**: the tarball unpacks to a package dir carrying a
   `tamias.yaml` (kind `files` or a future `binary` kind) and a `bin/`
   payload — binaries land in the store like any content.
3. **PATH reach**: a small `tamias bin` verb links/copies a package's
   `bin/` payload into `~/.local/bin` (symlink preferred; the store
   entry dir `<name>@<version>` is stable only when `version:` is
   declared — links must resolve through a stable alias, not the raw
   versioned path). Provider `services[].exec` may then name the bare
   binary. Alternative considered and deferred: exec'ing store-relative
   absolute paths — brittle across upgrades, leaks store layout into
   manifests.
4. **Self-update**: `tamias upgrade` is two paths. For *packages*, it
   already exists — `update` re-resolves the recorded ref (which for a
   binary package is the pinned https ref). For *tamias itself*, a
   `tamias self-upgrade` verb fetches its own published pinned ref,
   verifies sha256, stages beside the current exe, and atomically
   renames with a `.bak` fallback. A running binary cannot replace
   itself portably; the rename dance is the honest v1.
5. **crates.io stays the source channel** (`cargo install tamias`);
   release tarballs are the *binary* channel. They are complements,
   not competitors — crates.io builds from source, releases ship
   prebuilt.

**Trust model (honest):** the sha256 pin gives *integrity*, not
*authenticity* — it proves the bytes match what the ref's author
pinned, and TLS proves the fetch came from github.com, but nothing yet
proves the release asset itself is what the project published. That
authenticity layer is T-013 (ssh-sign/cosign over git:/registry refs);
for https tarballs the pragmatic v1 pin-sources are (a) the operator's
hand-written ref, (b) a checked-in lockfile-style pin per deployment,
(c) later, signed release metadata from the `cf:` registry. Registry-
carried packages already verify signatures + trust tiers today;
extending that to https tarballs (detached sigs alongside the asset)
is the honest v2 — an unsigned pinned tarball is still strictly better
than "no binary channel at all".

## Consequences

- **No grammar change.** `https:` + `#sha256:` covers v1; a `bin:` or
  `artifact:` scheme earns its place only if the `cf:` registry needs
  per-target negotiation (`<triple>` selection) or detached
  signatures — defer until a real consumer asks.
- **cargo-dist config must emit `.tar.gz`** (or the http kind learns
  `.tar.xz` — small, but a real change; open question below).
- **Per-target selection** is the manifest's problem in v1: one
  package per `<triple>` tarball, or a meta-package whose deps name
  the right ref. cargo-dist's `targets = [host]` already limits
  artifacts per repo.
- **`tamias bin` + `self-upgrade` are new verbs** — small, but they
  are code, not just docs. This ADR decides the *channel*; the verbs
  are follow-up tasks (`tamias bin` in flight).
- **Registry (`cf:`) becomes the natural v2**: index entries can carry
  per-target artifacts + signatures in one record instead of N pinned
  URLs.

## Open questions

- cargo-dist archive format: does dist emit `.tar.gz` for our config,
  or must `tamias-source-http` learn `.tar.xz`/`.zip`?
- Where do `bin/` links live — `~/.local/bin` fixed, or
  `$TAMIAS_HOME/bin` on PATH? (Fixed `~/.local/bin` matches the
  `xtask install` convention.)
- Does `tamias self-upgrade` also cover `tamias-serve`/driver bins,
  or only the `tamias` binary?
- Signature format when T-013 lands: sigstore bundle vs ssh-sign —
  does the https channel verify detached sigs fetched from the same
  release page, or only registry-carried ones?
