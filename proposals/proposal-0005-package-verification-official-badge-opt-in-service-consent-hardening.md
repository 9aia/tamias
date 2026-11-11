+++
id = "P-005"
title = "package verification + official badge; opt-in service consent hardening"
status = "done"
created = "2026-11-10"
owner = "devin"
priority = "med"
labels = ["provenance", "security"]
+++

## Problem

Tamias already has the provenance *machinery* — digest pins on
`http(s)`/`git:` refs, the content-addressed fetch cache with commit-time
integrity metadata, `ssh-keygen -Y` package signatures riding the
registry index (`publish --sign`), and `signing.allowed_signers` /
`signing.required` enforcement in the install pipeline. What it lacks
is a **user-facing trust tier**: today a package signed by the official
9aia release key and one signed by a random community key look identical
in `tamias list`/`info`/`verify`, and an unpinned moving ref looks the
same as a digest-pinned one until you read `source_ref` closely.

Two related gaps:

1. **No "official" concept.** `allowed_signers` is binary — a key is
   either authorized or not. There's no way to say "this signature is
   valid AND the signer is the official publisher", which is what a
   badge should mean. npm's `verified publisher` / sigstore-style
   identity, crates.io's verified badge — same user need.
2. **Opt-in services need a documented, enforceable story.** Install
   currently *does* execute code: the kind driver's `install` verb runs
   for any non-`files` package whose driver resolves, and bundled
   `drivers/` are consent-gated but PATH/`drivers/` drivers are trusted
   implicitly. `provider:` services are already opt-in (deploy writes
   units but never enables them; ingress hooks stay manual) — good —
   but the consent surface deserves tightening and a dry-run so users
   can see what an install *will* execute before consenting.

## Proposal

### A. Trust tiers — computed, never self-declared

Every package gets a trust tier derived from verified facts, in
ascending order:

| Tier | Meaning |
| --- | --- |
| `unpinned` | moving ref (`main`, `latest`, unversioned `cf:`), no digest or signature |
| `digest` | `#sha256:`/`#<sha>` pin or resolved-sha record verified |
| `signed` | signature present and verified against `allowed_signers` |
| `official` | signature verified against the **official** allowlist |

The tier is *computed* at install/verify time — a package can never
assert its own badge (same rule as provenance: the record is the
receipt).

### B. The official allowlist

- New config: `signing.official_signers` (path to an
  `allowed_signers`-format file) + `TAMIAS_OFFICIAL_SIGNERS` env
  override, resolved like `allowed_signers` (env → config →
  `~/.config/tamias/official_signers` when present).
- Rationale for a *separate* file: users can trust community signers
  (`allowed_signers`) without granting them official status. Official
  is a strictly smaller set.
- A sensible default ships documented: the 9aia release key(s), so
  first-party packages badge out of the box once the user installs the
  published `allowed_signers` snippet.
- **Principal granularity fix (required for this to be meaningful):**
  today `ssh-keygen -Y verify -I <principal>` uses the *bare* package
  name, so `@acme/w` and `@other/w` share signer authorization.
  `signing_bytes` already binds scope, so extend the principal to
  `@scope/name` (bare `name` for unscoped) — one-line change in
  `signature_status`/`enforce_signature`, backward-compatible since
  unscoped packages keep the bare principal.

### C. Surfaces

- `StoreRecord` gains `signer: Option<String>` (the verified principal)
  and `trust: Option<String>` — recorded at install, recomputed by
  `tamias verify`.
- `RegistryEntry` gains optional `publisher`/`signer` fields — *display
  metadata only*; the badge is always recomputed from the signature,
  never trusted from the index.
- CLI: `tamias list`/`info`/`search` render a badge column
  (`official`/`signed`/`pinned`/`unsigned`); `--json` emits
  `"trust": "…", "signer": "…"`. `tamias verify` prints
  `signature: verified (official)` style output and gains
  `--require-official` for CI gates.
- TUI: badge chip in the packages/services panes; `tamias doctor`
  warns when installed official-tier packages fail re-verification.
- Docs site: a "Provenance & trust" explanation page (Diátaxis)
  documenting the tiers and how to bootstrap `official_signers`.

### D. Opt-in services — tighten the consent story

Confirmed current posture (audit-verified): `tamias install` never
deploys units, never enables services, never registers forge hooks.
`tamias deploy` writes units *disabled*; `tamias service enable` and
`tamias run` are separate explicit verbs. What to add:

1. **Validate `type:`/`kind`.** The manifest leaves `kind` free-form,
   and it lands in `tamias-driver-{kind}` path joins for driver
   lookup — constrain it to `[a-z0-9][a-z0-9._-]*` in
   `Manifest::check` (same spirit as `is_valid_name`).
2. **`tamias install --plan` (dry-run).** Resolve ref → manifest →
   list what *would* happen: fetches (incl. transitive `include:`
   refs), driver verbs that would dispatch, bundled drivers that would
   need consent, provider services that exist (deploy stays manual).
   Prints the plan, executes nothing. This is the transparency layer
   opt-in consent needs.
3. **Per-package driver consent memory.** `--allow-drivers` is
   all-or-nothing per invocation. Record consented driver digests in
   `~/.config/tamias/consented.json` so a *re-install of identical
   bytes* doesn't re-prompt while a changed payload does — mirrors how
   package managers handle install scripts.
4. **Document `TAMIAS_ALLOW_DRIVERS=1` as a loaded gun** — it's honored
   by CLI, `tamias serve` (`allow_drivers`), and the TUI. Keep it, but
   docs + `doctor` should flag it when set.

### E. Non-goals

- No self-signed "official" claims, no registry-declared badges as
  authority, no new signature format (ssh-keygen stays).
- No auto-deploy of any kind — services stay strictly opt-in.
- cosign/OCI identity is future work (already noted in `tamias-sign`).

## Security audit context (2026-11-10)

A read-through of the install/fetch/store/serve surface found the
posture healthy: tar-slip and symlink escapes rejected, cache entries
integrity-checked on hit, store swaps atomic, all subprocess calls
timeout-bounded, signatures fail closed. Actionable findings filed
separately (see linked issue): unvalidated `kind` charset, uncompressed
untar size, transitive `include:` fetch semantics, `http://` registry
bases, serve-surface trust notes.
