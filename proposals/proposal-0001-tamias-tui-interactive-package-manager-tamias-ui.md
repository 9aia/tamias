+++
id = "P-001"
title = "tamias TUI — interactive package manager (`tamias ui`)"
status = "open"
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
- **Thin front-end, not a shell-out**: the TUI calls the same library ops
  the verbs do — `tamias-install::install` for installs, `tamias-store-local`
  `Store` for list/info/verify/remove — with `Env { store_root,
  cache_root }` built the same way `tamias serve`/`Env::from_env` does.
  No subprocess, no re-parsed CLI output. Where an op only exists as CLI
  code today (search, update, pin, cache clean), extract it into a shared
  lib surface the verb and the TUI both call — this *advances* T-014
  (dedupe the install pipeline) rather than adding a fourth divergent copy.

## UX sketch

Panes/tabs:

- **Installed** — store entries: name, type, source ref, resolved sha,
  digest prefix, size. `/` filters as you type (client-side over the
  in-memory entry list — store is a small local dir, so this is O(n)
  and instant; no index needed). `Enter` opens a detail pane: full manifest,
  provenance record, provider/services, bundled drivers, verify status.
- **Registry** — search-as-you-type against `tamias search`'s index
  (debounced; local registry = instant, remote = one request per pause).
  Results show name/scope/type/version/description; `i` installs the
  selected ref.
- **Actions** — verbs bound to keys + `:` palette:
  `i` install-by-ref (prompt), `r` remove, `v` verify, `u` update,
  `p` pin, `c` cache clean, `d` doctor. Destructive ops (`r`, `u` on a
  moving ref) confirm via modal; installs that ship bundled `drivers/`
  raise the same consent prompt the CLI's `--allow-drivers` flag gates —
  answered `y`/`n` in the TUI, never a hidden env var.
- **Progress** — installs run on a worker thread; the UI streams
  fetch/store/verify phases into a log pane with a spinner. `tamias-serve`'s
  silent `Surface::Rpc` hints don't apply — the TUI wants the verbose
  progress surface.
- **Keymap** — vim-style per myco-tui: `j/k` move, `Tab`/`1`/`2`/`3`
  switch panes, `/` filter, `:` commands (`:install <ref>`, `:remove <name>`,
  `:q`), `?` help, `R` reload, `q`/`Ctrl-C` quit.

## Non-goals (v1)

- `publish`/`bundle pack` authoring flows — read/search/install/manage only.
- `service`/`run`/`logs`/`deploy` provider lifecycle — revisit once the
  package-management core is proven; these are daemon-adjacent.
- Editing `tamias.yaml` deps — `add` stays a CLI verb for now.
- Multi-registry browsing beyond the configured default + `--registry`.

## Open questions

1. Verb name: `tamias ui` (myco parity) vs `tamias tui` vs `tamias manage`.
2. Should the TUI live behind the `tui` cargo feature (myco pattern) or
   ship unconditionally? Feature-gate keeps `cargo install tamias
   --no-default-features` lean; default-on matches `myco ui` UX.
3. Extend `tamias-serve` ops (search/update/pin) vs. a new shared ops lib —
   serve's `Env` is store/cache only; registry-aware ops need a wider Env.
   Leaning: widen `tamias-serve::Env` (or a sibling `tamias-ops`) so CLI,
   serve, and TUI share one dispatch.
4. Does T-014 (shared install pipeline extraction) land first, or does the
   TUI's shared-ops extraction subsume it?

## Decisions (2026-11-10)

1. Verb: **`tamias ui`** (myco parity).
2. Gate: **`tui` cargo feature, on by default** — `--no-default-features`
   keeps the lean binary.
3. Ops surface: **extend `tamias-serve`** — widen `Env` to be
   registry-aware so CLI verbs, `tamias serve`, and `tamias ui` share one
   dispatch; no new `tamias-ops` crate.

## Scope expansion (2026-11-10)

Non-goals removed — v1 now also covers:

- **Provider lifecycle**: `service` (list/start/stop/restart unit ops),
  `run` (foreground exec — spawn in a PTY pane or tail view), `logs`
  (journal tail + follow), `deploy`/`uninstall` provider teardown.
  These are daemon-adjacent but are first-class tamias verbs; the TUI
  is the natural console for them.
- **`tamias.yml` editing**: deps editor as a form — list `deps:` entries
  with add/remove/ref-edit rows; writes the project manifest via the
  same code path `tamias add` uses (manifest + lock update together, no
  hand-edited YAML drift).
- **Authoring**: `bundle pack <dir>` and `publish <dir> --registry <ref>`
  — dir picker/prompt, registry picker, progress pane reuse.
