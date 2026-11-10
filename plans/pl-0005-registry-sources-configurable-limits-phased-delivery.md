+++
id = "PL-005"
title = "registry sources + configurable limits — phased delivery"
status = "open"
created = "2026-11-10"
owner = "devin"
priority = "high"
blocks = ["PL-004"]
links = ["P-002"]
+++

Proposal: P-002. Prerequisite for PL-004 (TUI) Phases 3 + 6 — the
registry pane and publish flow consume these ops. Also advances T-015's
"tamias-config parsed never applied" by making `registries`/`publish`
actually route.

## Phase 0 — config model + name resolution

- [ ] `tamias-config`: add `limits` (`max_upload` bytes/human-string,
      defaults const) + per-registry `limits` override + per-package
      override map (name/glob → limits); `search.default` field.
- [ ] yml/toml formats: parse + render round-trip for new fields
      (render already exists — extend, don't fork).
- [ ] `open_registry` resolution: name → `registries[name]` (kind/url/
      token_env) → fallback raw dir/http via `parse_registry_ref`.
      Apply to `publish`, `search`, `registry serve --registry`? (serve
      stays local-dir; note in review).
- [ ] Routing defaults: bare `publish` → `publish.default`; bare
      `search` → `search.default` → `publish.default` → local root.
      Env override (`TAMIAS_REGISTRY`) wins over config, matching the
      store/cache env-wins rule.

## Phase 1 — CLI management verbs

- [ ] `tamias registry add <name> --kind … --url … [--token-env …]`
- [ ] `tamias registry remove <name>` / `get <name>` / `list`
      (table + `--json`).
- [ ] All writes via `ConfigFormat::render` into the discovered
      `tamias.{yml,yaml,toml}`; `--file`/`TAMIAS_CONFIG` honored;
      refuse to write when only defaults exist (prompt to create).
- [ ] `doctor` extended: check each configured registry resolves +
      reachable (entries() probe), report limits summary.
- [ ] `tamias schema`/`manifest` regenerated; docs reference updated.

## Phase 2 — limits enforcement (local + client)

- [ ] `tamias publish` preflight: resolve effective limit (per-pkg →
      per-registry → global → const), stat payload tarball, reject
      early with size/limit in the error.
- [ ] `tamias-registry-local` HTTP PUT: replace const `MAX_PUT` with
      configured limit — source: registry's own config file when serving
      from a project, `--max-upload` flag, else global default.
- [ ] `tamias registry serve` gains `--max-upload`/`--config` plumbing.
- [ ] Fetch side: `tamias-source-http`/registry-cf fetch cap reads the
      configured `max_download`/`max_tarball` (default 512 MiB).

## Phase 3 — CF worker limits

- [ ] `deploy-registry` (`tamias-deploy-cf`): bake configured limits
      into the generated Worker (env var / wrangler var / code const —
      pick at impl; worker must enforce without client config).
- [ ] Worker PUT path rejects over-limit uploads; error surfaces
      through `tamias publish` stderr cleanly.
- [ ] `tamias-registry-cf` client preflights against the same limits
      when known (from config) — cosmetic, server is authoritative.

## Phase 4 — serve ops (TUI prerequisite complete)

- [ ] `tamias-serve` ops: `registry.list`, `registry.add`,
      `registry.remove`, `registry.get`; `search`/`publish` accept
      `registry` as name-or-ref (resolution shared with CLI).
- [ ] Widened `Env` carries config/registry context (this is the same
      widening PL-004 Phase 0 needs — land together or sequence:
      P-002 Phase 4 ≡ PL-004 Phase 0 ops work).
- [ ] PTY/integration tests: add→publish→search round-trip by name;
      over-limit publish rejected client- and server-side.

## Phase 5 — polish

- [ ] `cargo xtask check`/`test` green; coverage on new config parse
      arms; docs (`docs/` reference + how-to "host your own registry").
