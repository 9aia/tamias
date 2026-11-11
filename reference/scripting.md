---
url: /tamias/reference/scripting.md
---
# Scripting tamias

Everything an agent or CI job needs without scraping human output.

## `--json`

These verbs emit JSON with `--json`:

| verb | shape |
| --- | --- |
| `list` | `{"packages": [entry…]}` — record fields + `label`, `size`, `dir` |
| `info <name>` | one entry object (same fields) |
| `verify <name>` | `{name, recorded, computed, ok, signature, trust, signer}` — non-zero exit on mismatch |
| `update [name]` | `{"updated": [label…]}` (with `--all` or a name) |
| `doctor` | per-check report — store, cache, git, registry, drivers |
| `config` | `{file, search, config}` — the effective config |
| `search <q>` | `{"results": [{label, name, scope, version, kind, digest, signed, publisher, file}]}` |
| `cache clean` | `{evicted, entries}` |

Without `--json` the same verbs print aligned text.

## Capability discovery

```sh
tamias manifest      # {"name","version","verbs":[…]} — verbs generated from the parser
tamias schema        # JSON Schema for the package manifest (tamias.yaml)
tamias gen-cli-docs  # this CLI reference, regenerated from clap
tamias completions bash|zsh|fish|elvish|powershell
```

`manifest`'s verb list can't drift — it's generated from the clap tree
each run.

## `tamias serve`

For stateful, interactive use, `tamias serve` speaks ndjson JSON-RPC
over stdio — see [the serve reference](/reference/serve).

## The engine as an ops manifest

`tamias-driver-tamias` (installed with tamias, discovered like any
driver) exposes the whole `tamias` CLI as a **pavo ops manifest** —
every bounded verb as a `cli.<verb>` wire query derived from the
engine's own clap tree, so generated surfaces (pavo's CLI/MCP
renderers) can't drift from the real verbs. Unbounded surfaces
(`serve`, `ui`, `registry serve`, `run`) are listed as `surfaces`,
never ops. Drive it over the driver's `serve` wire:

```sh
tamias-driver-tamias serve <<'EOF'
{"op":"query","query":"ops"}
{"op":"query","query":"cli.list","params":{}}
EOF
```

`query:"cli.<verb>"` params map by kind — `arg` = positional,
`opt` = `--name <v>`, `flag` = `--name`. `tamias-driver-tamias run <argv…>` is the raw passthrough (`run list` ≡ `tamias list`). Secret
params should ride stdin, not argv — generated CLIs accept
`--params-stdin` (a JSON object merged over the flags) so tokens stay
out of shell history.

## Environment

| var | effect |
| --- | --- |
| `TAMIAS_HOME` | store + `drivers/` root (default `~/.local/share/tamias`) |
| `TAMIAS_CACHE` | fetch-cache root (default `~/.cache/tamias/fetch`) |
| `TAMIAS_BIN_DIR` | bin dir for `tamias bin` deploys (default `$XDG_BIN_HOME` → `~/.local/bin`) |
| `TAMIAS_REGISTRY` | default registry root for `publish`/`search` |
| `TAMIAS_CONFIG` | explicit config file — replaces discovery for that invocation |
| `TAMIAS_ALLOW_DRIVERS` | `=1` consents to bundled `drivers/` (same as `--allow-drivers`) |
| `TAMIAS_CONSENT_FILE` | override the per-package driver-consent memory path |
| `TAMIAS_REGISTRY_TOKEN` | bearer token for http(s) registries |
| `TAMIAS_REGISTRY_SERVE_TOKEN` | require auth on `tamias registry serve` |
| `TAMIAS_SIGNING_KEY` / `TAMIAS_ALLOWED_SIGNERS` / `TAMIAS_OFFICIAL_SIGNERS` | `publish --sign` key + signature-trust files (names, never values inline) |
| `TAMIAS_OTEL_*` / `OTEL_*` | opt-in telemetry — see [Architecture](/explanation/architecture#observability) |

Config discovery is **layered**: the user-level
`~/.config/tamias/tamias.{yml,yaml,toml}` is the base and a
`./tamias.{yml,yaml,toml}` in the cwd overlays it per-field — project
wins, global fills the gaps. `--file`/`TAMIAS_CONFIG` replaces
discovery with that one file. Registry *writes* (`tamias registry
add|remove`) stay file-scoped — they edit the project `tamias.*` (or
`--file` target), never the user layer implicitly.

## Exit codes

A failed verb exits non-zero. `verify` fails on digest mismatch;
`doctor` fails if any check fails (the report still prints first).
`cache clean --all` and `--days` conflict — pick one.
