+++
id = "ADR-0004"
title = "sealed secrets packs — age-sealed payloads; tamias stays zero-knowledge, cuora owns seal/open"
status = "done"
created = "2026-11-12"
owner = "devin"
priority = "high"
labels = ["architecture"]
+++

# sealed secrets packs — age-sealed payloads; tamias stays zero-knowledge, cuora owns seal/open

Status: **proposed** (design spec for org-ledger P-027)

## Context

Three trust domains need secret stores — personal (`luisfuturist`),
org (`9aia`), external (`ktbhealth`). Today cuora vaults are
local/config-scoped and the registry token already demonstrated the
ambiguity problem (personal dev vault works, org prod vault 401s).

Tamias already gives us the right chassis: packs are versioned,
digest-pinned, optionally ssh-signed, stored as opaque payload trees,
and consumed through pluggable content-type drivers. What it lacks is
a **confidentiality primitive**: a way for a pack's payload to be
ciphertext that tamias — engine, store, cache, registry, TUI, logs —
can carry, verify, and dispense without ever reading.

This ADR decides the sealing format, the manifest semantics, and the
division of labor (tamias = transport/integrity, cuora =
seal/open/materialize). It is design-only; implementation tasks are
listed under Phases.

## Threat model

**What tamias learns (accepted, unavoidable):**

- Ciphertext bytes — opaque blobs in store/cache/registry.
- Payload *shape*: file names, file sizes, file count (the walk that
  computes the digest sees paths and lengths).
- Timing and access patterns: when a pack is installed/verified, when
  a consumer opens a secret.
- Pack metadata: name, scope, version, description, `sealed:` block
  (format + file map), digest, signature, source ref.
- Recipient *public* keys — public by design.

**What tamias must never learn:**

- Plaintext secret values. No decrypt path in the engine, no logging
  of opened values, no content indexing of sealed payloads, no
  plaintext in argv/env/manifests — the existing "secret names never
  values" rule extended to "secret *contents* never values".

**Adversaries:**

1. **Registry/store operator** — read access to the hosted registry
   and any mirror. Gets ciphertext + metadata only.
2. **Tamias itself / agent tooling** — same view; the TUI, `verify`,
   `list`, telemetry must treat sealed payloads as opaque.
3. **Other pack consumers** — a host that installs `secrets-9aia`
   holds ciphertext; without a recipient private key it learns
   nothing beyond shape.
4. **Network eavesdropper** — ciphertext over TLS; age adds
   authenticated encryption per file.

**Explicitly out of scope:**

- Metadata leakage (pack names like `secrets-9aia` reveal that a
  domain has secrets; sizes leak rough content length). Mitigation is
  operational (naming, padding) not cryptographic in v1.
- Post-compromise secrecy: anyone holding a recipient private key +
  old pack versions reads those versions forever. Rotation limits
  exposure going forward; it cannot un-see ciphertext.
- Tamias-side decryption under any flag. There is no
  `tamias open --key` — opening is cuora's job, in the consumer's
  context.

## Decision

### 1. Sealing format: **age** (rage), per-file, native X25519 recipients

Evaluated:

| Option | Verdict |
| --- | --- |
| **age** (`age` crate 0.12.x, str4d/rage — ~7.9M downloads, MIT/Apache, MSRV 1.74) | **Chosen.** Small audited format (X25519 + ChaCha20-Poly1305 STREAM), explicit recipients in a compact header, no config surface, excellent Rust crate, `ssh` feature for ssh-ed25519 recipients later. |
| sops | Rejected for v1. Built for YAML/JSON *trees* with KMS/Vault/age providers — we seal arbitrary files, not structured docs; pulls in a heavy dep tree and a different mental model. Revisit if KMS-backed recipients become a requirement. |
| Custom X25519 | Rejected. Never roll crypto; age *is* the minimal correct construction. |

**Layout:** each logical secret is one file in the payload, stored as
an age-encrypted file. Inside the payload dir:

```
<payload>/
  sealed/
    recipients            # age public keys, one per line (X25519; ssh-ed25519 later)
    registry-token.age    # age ciphertext of the secret bytes
    db-dsn.age
  tamias.yaml             # manifest, sealed: block maps logical → file
```

- **Per-file sealing**, not one archive blob: preserves per-file store
  addressing, per-file digest/verify, partial materialization, and
  lets cuora open one secret without decrypting the pack.
- **Recipients live inside the payload** (`sealed/recipients`), not
  only in the manifest: the payload digest — and therefore
  `tamias-sign`'s signature — covers them. A tampered recipient list
  breaks the digest. The manifest `sealed:` block *references* the
  map; it does not duplicate key material.
- **Nonce handling:** none needed — age generates a fresh file key +
  nonce per encryption; callers never supply one. Re-sealing the same
  plaintext yields different ciphertext (good: no equality oracle).
- Header magic `age-encryption.org/v1` is the ciphertext marker tamias
  validates against.

### 2. Manifest semantics

New optional manifest block (works with any `type:` — `sealed` is
orthogonal to content type; `type: secrets` is a *convention* paired
with the cuora driver, not a requirement):

```yaml
name: secrets-9aia
version: 1.4.0
type: secrets            # convention; driver-owned
sealed:
  format: age/v1
  recipients: sealed/recipients     # payload-relative path
  files:
    registry-token: sealed/registry-token.age
    db-dsn: sealed/db-dsn.age
```

Rules:

- `sealed` absent → pack is plaintext, everything as today.
- `sealed` present → **ciphertext-only pack**:
  - Every payload file except `tamias.yaml` and the declared
    `sealed.files` values + `sealed.recipients` target must end in
    `.age` and carry the age magic — enforced at **publish**
    (`tamias-publish` refuses) and again at **install**
    (`tamias-install` hard-errors). No warn-mode: a sealed pack with
    stray plaintext is a bug, not a preference.
  - `include:`, `bins:`, `provider:` are **rejected** on sealed packs
    in v1 — composition, bin deploys, and service specs over
    ciphertext are meaningless and would leak intent.
  - `deps:` still allowed (a sealed pack may depend on plaintext
    packs).
- **Digest/verify unchanged**: the store digest already covers the
  payload tree bytes → it digests ciphertext. `tamias verify`
  recomputes exactly as today. `tamias-sign` signs
  name/scope/version/payload-digest → the signature authenticates the
  ciphertext *and* the recipient list (via the payload digest).
  Authenticated + confidential, no new signing surface.
- `tamias list`/`info`/TUI show sealed packs with a `sealed` badge and
  sizes; they never attempt to read payload contents (they don't
  today either — this is a no-op confirmation, not new code).

### 3. cuora's role — the expertise half

Tamias stays zero-knowledge; **cuora owns everything that touches
keys or plaintext**:

- `tamias-driver-cuora` (new driver, `type: secrets` capability)
  exposes ops: `seal` (author-side: plaintext dir → sealed pack
  payload), `open` (one logical name → plaintext on stdout/file, in
  the consumer's context), `materialize` (open all → tmpfs/ramfs
  dir), `recipients` (show the pack's recipient set).
- `cuora://` refs resolve *through* sealed packs at consumption time —
  the ref names a logical secret (`cuora://secrets-9aia/db-dsn`), the
  driver finds the installed pack, opens the `.age` file with the
  local private key, returns the value to the caller's env/exec
  context. The tamias store only ever held ciphertext.
- Driver discovery is the existing mechanism (`tamias-driver-<type>`
  on PATH / `<tamias-home>/drivers/`); no engine changes beyond the
  sealed-payload validation above.

**Bootstrap (first secret):** sealing is *author-side*. The operator
generates a domain keypair locally (`rage-keygen` / `age` crate),
writes the public key into `sealed/recipients`, seals secrets with
the private key, and publishes the pack. Tamias is never in the
plaintext path at any point — there is no bootstrap paradox.

### 4. Key management

- **Recipient model:** one keypair per trust domain
  (`secrets-luisfuturist`, `secrets-9aia`, `secrets-ktbhealth`); a
  pack may list multiple recipients (shared access, break-glass
  second key). Per-pack keys are *not* required — domain keys keep
  operational cost low; revisit if blast radius demands it.
- **Rotation = new pack version.** Re-seal all files to the new
  recipient set, bump version, publish. Old versions remain readable
  by old keys — acceptable; revocation is "stop distributing old
  versions + rotate," not cryptographic erasure.
- **Private key storage, ranked:**
  1. **Age keyfile** `keys.txt` (mode 0600) under the consumer's
     cuora/tamias config dir — simplest, scriptable, matches age/rage
     conventions. **v1 choice.**
  2. OS keyring (`keyring`/`secret-service` crate) — better at-rest
     protection, adds deps + headless/CI friction. Phase-2 option.
  3. ssh-ed25519 recipients reusing existing SSH keys (`age` `ssh`
     feature) — zero new key material for operators already on SSH;
     worth supporting as a *recipient type* even in v1 if cheap.
  4. Hardware tokens (age plugin / FIDO) — out of scope.
- Private keys never enter packs, manifests, the store, or the
  registry. The `sealed/recipients` file carries public keys only.

## Phases

1. **This ADR** — format + semantics decided (this doc).
2. **T-sealed-1: manifest + publish path** — `sealed:` block in
   `tamias-core` manifest schema, `tamias-publish` ciphertext-only
   validation, docs.
3. **T-sealed-2: install path** — `tamias-install` sealed validation
   (reject stray plaintext / missing `.age` files), store/cache treat
   payloads as opaque (verify no code path reads contents),
   `sealed` badge in list/info/TUI.
4. **T-sealed-3: cuora driver** — `tamias-driver-cuora` with
   seal/open/materialize/recipients ops; `cuora://` resolution over
   installed packs; keyfile loading.
5. **T-sealed-4: dogfood** — `secrets-9aia` pack carrying the
   tamias registry token; migrate `tamias/registry-token` off the
   ambiguous vault env.

## Open questions (need owner decision)

1. **Crypto choice** — age recommended; confirm vs sops-if-KMS is a
   near-term requirement.
2. **Key storage** — keyfile v1 recommended; is OS-keyring a
   phase-2 must or a never?
3. **Orthogonality** — `sealed:` usable with any `type:` (recommended)
   vs hard-bound to `type: secrets`?
4. **ssh-ed25519 recipients in v1** — include the `age` `ssh` feature
   now, or defer?
5. **Multiple recipients UX** — who operates re-seal on rotation
   (operator locally vs a `cuora seal` authoring flow in the driver)?
