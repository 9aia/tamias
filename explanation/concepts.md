---
url: /tamias/explanation/concepts.md
---
# Concepts

The vocabulary tamias uses — each term defined once.

## Package and ref

* **Package** — *bytes + a manifest* (`tamias.yaml`). The manifest names
  the package (`name`, `version`, `@scope`), its content `type`, and
  optional `deps:`, `include:`, `provider:`, and `bins[]` declarations.
* **Ref** — the address of a package source: a local path, a `git:`
  clone URL with `//subdir` and `#rev`, an `http(s):` tarball (optionally
  `#sha256:`-pinned), a `cf:` registry ref, or a named-registry
  `<label>:<pkg>@<ver>`. See [Ref grammar](/reference/refs).
* **Resolved sha** — what a ref pinned to at install time: a commit sha
  for git, a payload sha256 for tarballs. Recorded on the store entry so
  provenance survives the ref moving.
* **Payload digest** — the content hash of the unpacked payload.
  `tamias verify` recomputes it; tampering with the store is detected.

## Stores

* **Install store** — `<tamias-home>/` entries of `name@version/` plus a
  record (source ref, resolved sha, digest, trust tier). What `list`,
  `info`, `verify` read.
* **Fetch cache** — content-addressed, keyed by resolved sha. Repeat
  installs and `update` reuse it; `remove` never touches it.
* **Registry** — where published packages live: a directory (optionally
  HTTP-served) or a Cloudflare Worker + R2/D1/KV. `publish`/`search`
  talk to it; installs read its `index.json`.
* **Bin dir** — where a package's executable surfaces land:
  `$TAMIAS_BIN_DIR` → `$XDG_BIN_HOME` → `~/.local/bin`.

## Content types and drivers

* **Content type** (`type:`/`kind`) — the *meaning* of a payload,
  declared as data in the manifest. The engine never links product
  code.
* **Driver** (`tamias-driver-<type>`) — the subprocess that knows how a
  type installs/deploys/uninstalls. Discovered on `PATH` or in
  `<tamias-home>/drivers/`; fails closed when absent.
* **Bundled driver** — a `drivers/` dir inside the payload. Arbitrary
  code → gated by `--allow-drivers` / consent memory.
* **Provider** — the manifest's declarative `provider:` block: services,
  ingress, verbs. `tamias deploy` renders it into per-user service
  units (systemd / launchd / Task Scheduler) without a driver.
* **`bins[]`** — declared executable surfaces: `shim: cli` (a generated
  script dispatching `tamias bin exec` → the kind driver) or
  `payload: tools/bin/<triple>/…` (a real binary, linked through a
  per-package `bin-alias` so upgrades don't dangle).

## Trust

* **Trust tier** — `official` / `signed` / `pinned` / `unpinned` /
  `bad`, *computed* at install/verify time from the ref's pin, the
  package signature, and your signers files. Never self-declared.
* **Signers files** — `allowed_signers` ("may sign packages I install")
  vs `official_signers` (the smaller curated set earning the `official`
  badge); `ssh-keygen -Y` format, namespace `tamias-package`.
* **Consent** — the `--allow-drivers` gate, remembered per payload
  digest in `~/.config/tamias/consented.json`. Binds to exact bytes, not
  the package name.
* **`include:`** — manifest composition: entries resolve to store
  payloads or recursively-installed refs; the *composed* tree is what
  drivers see, what gets digested, and what the store commits.
