+++
id = "P-002"
title = "registry sources + configurable limits — named registries, self-hosting, per-pkg max-upload"
status = "claimed"
created = "2026-11-10"
owner = "devin"
priority = "high"
labels = ["registry"]
blocks = ["P-001"]
links = ["P-001", "PL-004"]
+++

## Problem

Two gaps block self-hosted, multi-registry operation — and both gate the
TUI (P-001/PL-004) registry pane and publish flow:

1. **Named registries are modeled but unreachable.** `tamias-config`
   already has `registries: {name → {kind, url, token_env}}` and
   `publish.default`, but they're *reserved*: only `doctor` reads
   `publish.default`, and `open_registry` takes a raw dir/http value —
   `--registry my-co` doesn't resolve. There is no `tamias registry
   add/remove/list`, so managing self-hosted registries means
   hand-editing `tamias.yml`. `search` has no default routing at all.
2. **Limits are hardcoded.** `tamias-registry-local` enforces a const
   `MAX_PUT`; the cf client/worker cap at `MAX_TARBALL` = 512 MiB. A
   self-hoster can't tune upload size (or future limits) per registry,
   per package, or globally — and the *server* side can't be configured
   at all for the local HTTP registry beyond the const.

## Proposal

### A. Registry sources — named, manageable, routable

- **CLI**: `tamias registry add <name> --kind local|http|cf --url <u>
  [--token-env VAR]`, `tamias registry remove <name>`,
  `tamias registry list`, `tamias registry get <name>`. Writes go through
  `ConfigFormat::render` (yml/toml impls already exist) — no hand-edited
  YAML, comments preserved where the format allows.
- **Resolution**: `--registry <value>` resolves a *name* first, then
  falls back to raw dir/http parsing (`parse_registry_ref` unchanged).
  Bare `tamias publish` routes via `publish.default`; bare `tamias
  search` gains the same routing (new `search.default`, falling back to
  `publish.default`, then the local default root). `token_env` stores
  the env var *name* only — values never touch config or disk.
- **Kinds**: today `Registry.kind` documents `local` (default) | `cf`,
  and transport is inferred — dir → `LocalRegistry`, http(s) URL →
  `HttpRegistry` (the `tamias-registry-cf` client, which serves *any*
  http endpoint incl. the worker). This proposal makes the kind set
  explicit: `local` | `http` | `cf`, with url-scheme inference kept as
  fallback so existing configs keep working. `cf` = worker-managed
  registry (R2 + bearer token); `http` = any self-hosted endpoint.
  Self-hosting = `local` + `tamias registry serve`, or `http`/`cf`.
- **Surfaces**: CLI verbs now; `tamias-serve` ops
  (`registry.list/add/remove`, name-aware `search`/`publish`) so the TUI
  registry pane (PL-004 Phase 3) and author pane (Phase 6) consume them
  instead of re-implementing.

### B. Configurable limits

- **Config model** (`tamias-config`): `limits` block —
  `max_upload` (bytes; human strings like `256MiB` accepted), plus
  room for `max_entries`/rate limits later. Resolution order:
  per-package override → per-registry override → global default →
  built-in const (today's behavior).
- **Per-package**: overrides keyed by package name, exact match or
  `*` glob — e.g. allow big `kuda-domain` packs but cap `files` payloads.
- **Enforcement points**:
  - *Client preflight* (`tamias publish`): stat the payload, reject
    before upload with a clear error — fail fast, no wasted transfer.
  - *Local registry server* (`tamias registry serve` / PUT handler):
    read limits from the same config (flag/`--limits` override for
    serving a registry you don't own the config of).
  - *CF worker*: `deploy-registry` bakes the configured limits into the
    generated Worker config/code — the worker enforces independently of
    any client.
- **Fetch side**: `MAX_TARBALL` fetch caps become configurable too
  (registry entry or global), defaulting to today's 512 MiB.

## Non-goals

- Registry authN/Z beyond the existing `token_env` bearer convention.
- Mirroring/sync between registries, quotas per user, billing.
- Changing the registry *index format* — limits are enforcement, not
  schema.

## Phasing

See PL-005. Roughly: config model + CLI management → local-server +
client enforcement → cf worker → serve ops for TUI consumption.
