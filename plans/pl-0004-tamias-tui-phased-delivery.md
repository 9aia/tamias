+++
id = "PL-004"
title = "tamias TUI — phased delivery"
status = "done"
created = "2026-11-10"
owner = "devin"
priority = "medium"
links = ["P-001"]
+++

Proposal: P-001. Precedent: `myco-tui` (ratatui 0.30 + crossterm 0.29 +
notify, `myco ui` behind `tui` feature). Decisions locked in P-001:
verb `tamias ui`, `tui` feature default-on, shared ops in widened
`tamias-serve`.

## Phase 0 — shared ops groundwork

> **Prerequisite**: P-002/PL-005 (registry sources + limits) lands
> first or alongside — the Registry pane (Phase 3) and Author pane
> (Phase 6) consume named registries + serve ops it defines. Phase 0's
> `tamias-serve` Env widening is shared work with PL-005 Phase 4 —
> sequence them together.

- [x] Resolve open questions (verb, gate, ops home) — see P-001 Decisions.
- [x] Add workspace deps: `ratatui = "0.30"`, `crossterm = "0.29"`,
      `notify = "8"`, `portable-pty = "0.9"` (dev-dep on tamias-tui).
- [x] Widen `tamias-serve`: `Env` gains registry + config context so ops
      cover `search`, `update`, `pin`, `cache clean` (in addition to
      existing install/list/info/verify/remove). CLI verbs and serve
      dispatch refactor onto it — scoped T-014 dedupe, no 4th copy.
- [x] Add serve/TUI-needed ops for later phases behind the same surface:
      `service list`, `run`, `logs`, `deploy`, `uninstall`,
      `manifest get/set-deps`, `bundle pack`, `publish` (can land
      per-phase instead — decide at extraction time).

## Phase 1 — skeleton + installed view (read-only)

- [x] `crates/tamias-tui`: `run(...)` mirroring `myco_tui::run` — alt
      screen, raw mode, panic hook, terminal Guard, crossterm loop,
      `notify` watcher on store root.
- [x] App model: installed entries via `tamias-store-local::Store`;
      `/` client-side filter; `j/k`/arrows, `Tab`/`1`-`5` panes, `?`
      help, `:` palette, `q`/`Ctrl-C`, mouse.
- [x] `tamias ui` verb in tamias-cli behind `tui` feature; env from
      `Env::from_env` + config.
- [x] PTY smoke test (`tests/pty.rs`): launch, draw, quit cleanly.

## Phase 2 — actions on installed packages

- [x] Detail pane: manifest, provenance, provider/services, drivers,
      verify status.
- [x] `v` verify, `u` update, `p` pin, `c` cache clean, `d` doctor —
      confirm modals where mutating; errors to status line, never panic.
- [x] `r` remove — confirm modal + non-`files` driver-teardown warning
      (same rule as serve's remove).
- [x] Worker-thread exec for blocking ops; spinner.

## Phase 3 — registry search + install

- [x] Registry pane: debounced search-as-you-type; `i` installs selected
      ref.
- [x] `i` install-by-ref prompt with grammar hint.
- [x] Bundled-driver consent modal → `allow_drivers` (no env var).
- [x] Progress pane: streamed phases via channel.

## Phase 4 — provider lifecycle

- [x] Services pane: `provider:` blocks + unit state table.
- [x] `run` exec — PTY pane or scrollback tail; `logs` follow view.
- [x] `deploy`/`uninstall` with confirms; extend tamias-serve ops as
      needed (or call tamias-deploy-local/-cf directly — pick at impl).

## Phase 5 — manifest editor (`tamias.yaml` deps form — the file `tamias add` writes)

- [x] Editable deps rows: add/remove/set-ref; commits through the same
      manifest+lock write path as `tamias add` (which writes
      `tamias.yaml` — match that, don't invent a second filename).
- [x] Read-only render of the rest of the manifest (no free-form YAML
      editing).

## Phase 6 — authoring

- [x] `bundle pack <dir>` flow: dir prompt → progress → output path.
- [x] `publish <dir> --registry <ref>` flow: dir + registry prompts →
      progress → result.

## Phase 7 — polish + ship

- [x] `R` reload + `notify` on store root and browsed registry index.
- [x] Help overlay + `:` palette cover every bound verb.
- [x] Docs: `docs/` how-to page; `gen-cli-docs` picks up `ui`;
      README mention.
- [x] `cargo xtask check` + `test` green; PTY tests cover terminal
      restore on quit paths.
