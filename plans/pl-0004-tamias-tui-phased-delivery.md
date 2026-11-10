+++
id = "PL-004"
title = "tamias TUI — phased delivery"
status = "open"
created = "2026-11-10"
owner = "devin"
priority = "medium"
links = ["P-001"]
+++

Proposal: P-001 (`proposals/proposal-0001-…`). Precedent: `myco-tui`
(ratatui 0.30 + crossterm 0.29 + notify, `myco ui` behind `tui` feature).

## Phase 0 — decisions + groundwork

- [ ] Resolve open questions in P-001: verb name (`ui` vs `tui`), feature
      gate default, shared-ops home (`tamias-serve::Env` widening vs new
      `tamias-ops`), ordering vs T-014.
- [ ] Add workspace deps: `ratatui = "0.30"`, `crossterm = "0.29"`,
      `notify = "8"`, `portable-pty = "0.9"` (dev-dep).
- [ ] Extract shared ops surface the CLI verbs, `tamias serve`, and the
      TUI all call: at minimum `list`/`info`/`verify`/`remove`/`install`
      (already in tamias-install), plus `search` (registry-aware Env),
      `update`, `pin`, `cache clean`. Do this as the T-014 dedupe or a
      scoped subset — no fourth copy of fetch→driver→store.

## Phase 1 — skeleton + installed view (read-only)

- [ ] `crates/tamias-tui` crate: `run(...)` entry mirroring
      `myco_tui::run` — alt-screen, raw mode, panic hook, terminal Guard,
      crossterm event loop, `notify` watcher on store root for
      auto-reload.
- [ ] App model: installed entries (name, type, source_ref, resolved,
      digest, size) loaded via `tamias-store-local::Store`; `/` filter
      (client-side substring, case-insensitive); `j/k`/arrows, `Tab`/`1-3`
      pane switch, `?` help overlay, `:` command line, `q`/`Ctrl-C` quit,
      mouse wheel + click.
- [ ] `tamias ui` verb in tamias-cli behind `tui` feature
      (`default = ["tui"]`; `Serve`-style env construction from
      `Env::from_env` + config).
- [ ] PTY smoke test (`tests/pty.rs`, portable-pty): launch, draw list,
      quit cleanly.

## Phase 2 — actions on installed packages

- [ ] Detail pane: full manifest, provenance (source_ref/resolved/digest),
      size, provider/services, drivers, last verify result.
- [ ] `v` verify, `u` update, `p` pin — confirm modal where mutating;
      errors surface in a status/log line, never panic.
- [ ] `r` remove — confirm modal naming the package + warning when a
      non-`files` kind driver teardown may leave provider resources
      (same rule as tamias-serve's remove).
- [ ] Worker-thread execution for any op that can block (verify on large
      payloads); spinner + cancellable where feasible.

## Phase 3 — install + registry search

- [ ] Registry pane: debounced search-as-you-type over the shared
      `search` op; columns name/scope/type/version/desc; `i` installs
      selected entry's ref.
- [ ] `i` install-by-ref prompt (grammar hint in placeholder:
      `./dir | git:…#rev | https://….tgz#sha256:… | cf:…`).
- [ ] Bundled-driver consent modal → maps to `allow_drivers` on the
      install call (no env var).
- [ ] Progress pane: stream install phases (fetch → verify → store →
      driver dispatch) from a channel fed by the worker thread.
- [ ] `c` cache clean (`--all`/days via `:` args), `d` doctor summary.

## Phase 4 — polish + ship

- [ ] `R` reload + `notify` watcher covering store root AND registry
      index dir if browsed.
- [ ] Help overlay documents full keymap; `:` palette covers every
      bound verb.
- [ ] Session/state persistence only if it earns its keep (myco-tui
      saves session; tamias likely stateless — decide in review).
- [ ] Docs: `docs/` page (how-to: "manage packages interactively"),
      `gen-cli-docs` picks up `ui` verb automatically; README mention.
- [ ] `cargo xtask check` + `test` green; PTY tests cover quit-path
      terminal restore (raw-mode leak regression).
