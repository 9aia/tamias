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

`Tab` / `1`–`5` switch between five panes:

* **Installed** — every store entry: name, type, source ref, resolved
  sha, digest, size. `Enter` opens a detail view (manifest, provenance,
  provider services); `Esc`/`←` goes back.
* **Registry** — search-as-you-type against a registry (debounced;
  `Enter` fires immediately). `/` edits the query, `i` installs the
  selected result. `:registry <name>` picks the target — a configured
  registry name or a raw dir/http ref; bare `:registry` resets to the
  default routing.
* **Services** — installed packages' `provider:` services with unit
  state. `s`/`S` start/stop, `r` restart, `e`/`E` enable/disable,
  `d` deploy, `x` uninstall (all confirmed). `t`/`T` tail/follow the
  journal, `n` runs the service in the foreground — `x` kills a live
  stream.
* **Manifest** — the project manifest's `deps:` as an editable table:
  `a` add a ref, `d`/`x` remove, `e` edit (installs the new ref, then
  swaps — a failed install leaves the manifest untouched). Writes go
  through the same manifest+lock path as `tamias add`. `s` (or
  `:sync`) reconciles the whole `deps:` set — `tamias sync`'s
  install-missing-or-drifted pass, behind a confirm.
* **Author** — `p` packs a payload dir (`bundle pack`), `P` publishes
  (dir + registry prompts, confirmed); `r` lists configured registries.

## Keys

Vim-style, same conventions as `myco ui`:

| Key | Action |
| --- | --- |
| `j`/`k`, arrows | move |
| `/` | filter (Installed) / query (Registry) |
| `i` | install-by-ref prompt (Installed) / install result (Registry) |
| `v` `u` `U` `p` `r` `c` `C` `d` | verify · update · update-all · pin · remove · cache clean (>30d) · cache clean all · doctor |
| `:` | command line — `:install <ref>`, `:remove <name>`, `:verify`, `:update`, `:pin`, `:search <q>`, `:registry`, `:run`, `:logs`, `:service`, `:deploy`, `:manifest`, `:sync`, `:registries`, `:reload`, `:q` |
| `?` | help overlay |
| `R` | reload |
| `q` / `Ctrl-C` | quit (kills any live service stream) |

Mutating verbs ask `y`/`n` first (default `n`). Installs that ship
bundled `drivers/` raise the consent prompt — answering `y` is the
equivalent of `--allow-drivers`; there is no env-var shortcut in the
TUI.

## Notes

* Long ops (install, publish, verify) run on a worker thread; phases
  stream into the log strip with a spinner in the status bar.
* The TUI watches the store root and reloads when it changes on disk —
  installs from other terminals show up without `R`.
* Everything the TUI does is also available as CLI verbs or
  `tamias serve` ops — the TUI is a front-end, not a second engine.
