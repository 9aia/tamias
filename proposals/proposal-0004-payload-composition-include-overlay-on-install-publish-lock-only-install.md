+++
id = "P-004"
title = "payload composition — include:/overlay() on install+publish, lock-only install"
status = "open"
created = "2026-11-10"
owner = "devin"
priority = "high"
labels = ["composition"]
+++

## Problem

Three composition/reproducibility gaps left by T-015's audit:

1. **`include:`/`overlay()` dead on install** — `Manifest.include`
   ("installed store names or installable source refs; local wins,
   earlier includes win") is parsed/displayed but nothing resolves it;
   `overlay()` deep-merge (yaml/json doc-merge, whole-file otherwise)
   has zero callers. Packages can't compose base packages today.
2. **No lock-only install** — `tamias.lock` records provenance but
   isn't a spec; `sync` re-resolves `deps:` refs. No way to install a
   project pinned to exact recorded shas/digests.
3. **Publish ignores `include:`** — `bundle pack`/`publish` tar up the
   payload dir as-is; a manifest with includes ships an uncompensated
   composition claim.

## Proposal

### A. Include resolution on install (the big piece)

In `tamias-install`, after manifest load and before driver dispatch:

- Resolve each `include:` entry in manifest order: store name → that
  entry's payload dir; installable ref → install it first (recursive —
  depth cap ~8 + cycle detection on (name,version)/(ref) pairs; a
  package including itself or an ancestor fails loudly).
- Compose layers with `overlay()`: bases in manifest order (earlier
  wins), local payload on top. Compose into a scratch dir, run driver
  dispatch + store commit against the COMPOSED tree; the store record's
  digest covers the composed payload (verify recomputes honestly).
  Manifest inside the composed payload keeps `include:` for provenance.
- Cache: composed digest is content-addressed — cache key stays the
  source payload digest? NO — composed result must be what's stored;
  cache stores the fetched raw payload (pre-compose) since compose is
  deterministic from store state. Document: include resolution happens
  post-cache, pre-driver.
- `include:` of a store name that's absent → error listing it (no
  implicit install of bare names? — actually allow refs AND names;
  names resolve store-only, missing name = hard error suggesting the
  ref form).

### B. Publish/pack include handling

- `tamias publish`/`bundle pack`: when manifest has `include:`,
  FLATTEN by default — resolve bases (same resolver as install,
  read-only: bases must be installed or fs-resolvable; publish does
  NOT install), overlay into the packed payload, strip nothing from
  the manifest (includes stay as provenance). `--keep-includes` flag
  ships the raw payload for consumers that compose themselves.
  Unresolvable base → publish fails with the base named.

### C. Lock-only install

- `tamias sync --from-lock` (name TBD at impl — alternatives:
  `tamias install --lock`, `tamias sync --locked` already exists for
  verify-only): iterate `tamias.lock` entries, install each recorded
  `source_ref` (which carries the pinned rev/digest), refresh nothing
  (lock is already the spec). Manifest `deps:` not consulted.
  Report per-entry like sync.

## Non-goals

- include: cycles beyond hard-error; include: of include: transitive
  closure is handled by recursion naturally (bases' own includes
  resolve when THEY installed — but a base installed pre-include-era
  has flat payload; composing uses what's stored. Document).
- Signed includes — signature covers composed digest; base sigs
  verified at base install time (already enforced there).
- include: in project manifests (deps:-only today; include: there is
  meaningless — document).
