---
url: /tamias/how-to/use-the-tui.md
---
# How to use the TUI

`tamias ui` opens an interactive console — the same operations as the
CLI verbs, without memorizing flags. It needs the `tui` cargo feature
(on by default; `cargo install tamias --no-default-features` builds the
lean binary without it).

```sh
tamias ui
```

## Panes

Six panes, tab-ordered for browsing first and search last —
`Tab`/`l` and `h`/`Shift-Tab` cycle, `1`–`6` jump directly
(`1` Home … `6` Registry). `Esc`/`q` closes an open package page
before it quits; pane switches close the page too.

* **Home** — the landing pane: wordmark, stats strip, and a
  recently-installed list. `j`/`k` + `g`/`G` walk the recents,
  `Enter` opens a package page, `/` jumps to registry search,
  `i` installs by ref.
* **Installed** — store entries collapsed to the latest version per
  package (`· N versions` counts the other copies the store keeps).
  `Enter` opens the package page; `/` filters client-side, `F`
  clears. `v` verify, `u`/`U` update one/all, `p` pin → new ref,
  `x` remove (confirm — on a multi-version group it opens a picker
  where space/`a`/`n`/`/` narrow the set and `⏎` removes it),
  `c`/`C` cache clean (>30d / all), `d` doctor, `i` install by ref,
  `R` reload.
* **Registry** — search-as-you-type against a registry (debounced;
  `Enter` fires immediately). Substring over name/scope/description/
  keywords ranks first, then fuzzy name-subsequence hits. Results
  mark `[signed]`, `installed <ver>`, and version counts.
  `Enter` opens the package page, `i` installs the result,
  `:registry <name>` picks the target (a configured name or a raw
  dir/http ref; bare `:registry` resets to default routing).
* **Services** — provider services with unit state, one row per
  unit (`×N` counts the store entries providing it). `s`/`S`
  start/stop, `!` restart, `e`/`E` enable/disable, `d` deploy,
  `x` uninstall (all confirmed), `t`/`T` tail/follow the journal,
  `n` runs the service in the foreground (`x` kills a live stream),
  `Enter` opens the newest provider's package page, `R` refreshes.
* **Project** — the project manifest's `deps:` as an editable
  table: `a` add a ref, `d`/`x` remove, `e` edit (installs the new
  ref, then swaps — a failed install leaves the manifest
  untouched). Writes go through the same manifest+lock path as
  `tamias add`. `s` (or `:sync`) reconciles `deps:` against the
  store + lock — `tamias sync`'s pass, behind a confirm; `S` (or
  `:sync --from-lock`) installs `tamias.lock` verbatim. `R`
  reloads the manifest.
* **Publish** — `p` packs a payload dir (`bundle pack`), `P`
  publishes (dir + registry prompts, confirmed), `r` lists
  configured registries (a table you can select from), `R`
  reloads the registry list.

## Package pages

`Enter` on an Installed row, a Registry result, a Home recent, or a
Services provider opens an npm-style package page: header chips,
description, install command, deps/services/meta, the installed
versions list, provenance last. `j`/`k` scroll, `g`/`G` jump to the
ends, the wheel scrolls, `y` copies the install ref. `v` focuses the
versions section — `j`/`k`/`g`/`G` pick one and `i` installs that
exact version (registry siblings or installed store copies — rollback
is an exact-version install, not an activate). Superseded copies are
dropped with `:prune`. `esc`/`q`/`h`/`←` closes the page and returns
to where `Enter` was pressed.

## Keys

| Key | Action |
| --- | --- |
| `j`/`k`, arrows, wheel | move / scroll |
| `g`/`G` | top / bottom |
| `Enter` | package page |
| `/` | filter (Installed) / search (Registry) / jump to registry search (Home) |
| `F` | clear filter |
| `Tab`/`l`, `h`/`Shift-Tab`, `1`–`6` | next / prev / jump pane |
| `:` | command line — `Tab` completes, `↑`/`↓` history, line editing (arrows, `Home`/`End`, `Ctrl-A`/`E`/`U`/`W`) |
| `L` or `:log` | op-log sidebar — `j`/`k` scroll, `g`/`G` ends, `q` close |
| `?` | help overlay (scrollable; `j`/`k`, `PgUp`/`PgDn`, `g`/`G`, wheel; `esc`/`?`/`q` close) |
| `R` | reload |
| `q` / `Ctrl-C` | quit (closes an open page first; `C-c` quits anywhere) |

Pane-gated verbs: Installed `v u U p x c C d i`, Services
`s S ! e E d x t T n R`, Project `a d x e s S R`, Publish
`p P r R`, Registry `i` (+ `v` on a detail page). Dead keys flash a
hint in the status bar.

The `:` command line accepts: `:q` `:r`/`:reload` `:help` `:log`
`:install <ref>` `:remove <name>` `:uninstall <name>` (provider
teardown) `:verify <name>` `:update [name] [--no-prune]`
`:pin <name> <ref>` `:search <q>` `:registry <name>` `:clean [all]`
`:doctor` `:services` `:service <verb> <unit>` `:run <pkg> [svc]`
`:logs <pkg|unit> [-f]` `:deploy <name>` `:manifest` `:pack <dir> [out]` `:publish <dir> [registry]` `:registries` `:sync [--from-lock]`
`:prune [--keep N]`.

Mouse: the wheel scrolls (the package page when open), a click
selects a row, a click on a tab switches panes.

Mutating verbs ask `y`/`n` first (default `n`). Installs that ship
bundled `drivers/` raise the consent prompt — answering `y` is the
equivalent of `--allow-drivers`; there is no env-var shortcut in the
TUI.

## Verify

`tamias ui` opens on the **Home** pane — wordmark, stats, and
recently-installed rows. `Tab` reaches **Installed**; `?` shows the
scrollable key overlay; `q` quits.

## Notes

* Long ops (install, publish, verify) run on a worker thread; phases
  stream into the right-side op log (`L`/`:log`) with a spinner in
  the status bar.
* The TUI watches the store root and reloads when it changes on disk —
  installs from other terminals show up without `R` (a slow rescan
  covers missed events).
* Everything the TUI does is also available as CLI verbs or
  `tamias serve` ops — the TUI is a front-end, not a second engine.
