+++
id = "P-001"
title = "tamias TUI — interactive package manager (`tamias ui`)"
status = "done"
created = "2026-11-10"
owner = "devin"
priority = "medium"
labels = ["tui"]
+++

## Problem

`tamias` is a CLI-only surface today: ~20 verbs across install/add/remove/
list/info/verify/update/pin/search/publish/registry/service/run/logs.
Every mutation requires knowing the verb, flag spellings, and ref grammar
(`git:…//sub//…#rev`, `#sha256:…`, `cf:…`). There is no interactive way to
*see* what's installed, search a registry, or manage packages — the natural
"package manager" UX (the thing `apt`/`brew`/`pacman -Ui`/`kuda ui` give
you) is missing. The `tamias-driver-tamias` pavo view is read-only and
agent-facing, not a human console.

## Proposal

Add `tamias-tui` — a ratatui/crossterm TUI shipped as `tamias ui`, mirroring
the proven `myco-tui` pattern in the org:

- **New crate** `crates/tamias-tui`, ported structurally from
  `myco/crates/myco-tui` (app model + render split, vim keymap, `:` command
  line, `?` help, mouse, PTY tests via `portable-pty`).
- **Feature gate**: `tamias-cli` gains optional dep + `tui` feature
  (`default = ["tui"]`, `--no-default-features` keeps the lean binary —
  same as myco). Workspace deps gain `ratatui = "0.30"`,
  `crossterm = "0.29"`, `notify`, `portable-pty` (dev).
- **Thin front-end, not a shell-out**: the TUI calls shared library ops —
  `tamias-install::install`, `tamias-store-local::Store`, and ops
  extracted into `tamias-serve` (see Decisions) — with `Env { store_root,
  cache_root, … }` built the same way `Env::from_env` does. No subprocess,
  no re-parsed CLI output, no fourth copy of fetch→driver→store: this
  *advances* T-014 (dedupe the install pipeline).

## UX sketch

Panes/tabs:

- **Installed** — store entries: name, type, source ref, resolved sha,
  digest prefix, size. `/` filters as you type (client-side over the
  in-memory entry list — store is a small local dir, so this is O(n)
  and instant; no index needed). `Enter` opens a detail pane: full manifest,
  provenance record, provider/services, bundled drivers, verify status.
- **Registry** — search-as-you-type against the shared `search` op
  (debounced; local registry = instant, remote = one request per pause).
  Results show name/scope/type/version/description; `i` installs the
  selected ref.
- **Services** — provider lifecycle: installed packages' `provider:`
  blocks, unit state, `run` exec (PTY pane or scrollback tail), `logs`
  follow view, `deploy`/`uninstall` with confirms.
- **Manifest** — the project manifest (`tamias.yaml`, the file
  `tamias add` writes; config discovery accepts `yml`/`yaml`/`toml`)
  `deps:` as an editable form: add/remove/set-ref rows; writes go
  through the same manifest+lock path as `tamias add`, never
  hand-edited drift.
- **Author** — `bundle pack <dir>` and `publish <dir> --registry <ref>`
  flows with dir/registry prompts and the shared progress pane.
- **Actions** — verbs bound to keys + `:` palette:
  `i` install-by-ref (prompt), `r` remove, `v` verify, `u` update,
  `p` pin, `c` cache clean, `d` doctor. Destructive ops (`r`, `u` on a
  moving ref) confirm via modal; installs that ship bundled `drivers/`
  raise the same consent prompt the CLI's `--allow-drivers` flag gates —
  answered `y`/`n` in the TUI, never a hidden env var.
- **Progress** — long ops (install, publish, verify on large payloads)
  run on a worker thread; the UI streams phases into a log pane with a
  spinner. `tamias-serve`'s silent `Surface::Rpc` hints don't apply —
  the TUI wants the verbose progress surface.
- **Keymap** — vim-style per myco-tui: `j/k` move, `Tab`/`1`-`5`
  switch panes, `/` filter, `:` commands (`:install <ref>`,
  `:remove <name>`, `:q`), `?` help, `R` reload, `q`/`Ctrl-C` quit.

## Non-goals (v1)

- Multi-registry browsing beyond the configured default + `--registry`.
- `tamias.yaml` editing beyond the `deps:` form (no schema-free YAML
  round-trip; the form covers the mutation `tamias add` performs).
- Daemon/multi-actor concerns — still peregrin's, per the boundary.

## Decisions (2026-11-10)

1. Verb: **`tamias ui`** (myco parity).
2. Gate: **`tui` cargo feature, on by default** — `--no-default-features`
   keeps the lean binary.
3. Ops surface: **extend `tamias-serve`** — widen `Env` to be
   registry-aware (and later service/authoring-aware) so CLI verbs,
   `tamias serve`, and `tamias ui` share one dispatch; no new
   `tamias-ops` crate.
4. Scope: **full surface** — provider lifecycle, `tamias.yml` deps form,
   and `bundle pack`/`publish` authoring are in v1, not deferred.
5. Prerequisite: **P-002** (registry sources + configurable limits)
   — the Registry/Author panes need named registries and serve ops it
   defines; lands first or alongside Phase 0.
6. T-014 ordering: the shared-ops extraction in Phase 0 *is* the T-014
   dedupe for the ops the TUI needs; remaining pipeline dedupe continues
   under T-014 independently.
