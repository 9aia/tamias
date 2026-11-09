# Scripting tamias

Everything an agent or CI job needs without scraping human output.

## `--json`

These verbs emit JSON with `--json`:

| verb | shape |
| --- | --- |
| `list` | `{"packages": [entry…]}` — record fields + `label`, `size`, `dir` |
| `info <name>` | one entry object (same fields) |
| `verify <name>` | `{name, recorded, computed, ok}` — non-zero exit on mismatch |
| `update [name]` | `{"updated": [label…]}` (with `--all` or a name) |
| `doctor` | per-check report — store, cache, git, registry, drivers |
| `config` | `{file, search, config}` — the effective config |
| `search <q>` | `{"results": [{label, name, scope, version, kind, digest, file}]}` |
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

## Environment

| var | effect |
| --- | --- |
| `TAMIAS_HOME` | store + `drivers/` root (default `~/.local/share/tamias`) |
| `TAMIAS_CACHE` | fetch-cache root (default `~/.cache/tamias/fetch`) |
| `TAMIAS_REGISTRY` | default registry root for `publish`/`search` |
| `TAMIAS_ALLOW_DRIVERS` | `=1` consents to bundled `drivers/` (same as `--allow-drivers`) |
| `TAMIAS_REGISTRY_TOKEN` | bearer token for http(s) registries |
| `TAMIAS_REGISTRY_SERVE_TOKEN` | require auth on `tamias registry serve` |
| `TAMIAS_OTEL_*` / `OTEL_*` | opt-in telemetry — see [Architecture](/explanation/architecture#observability) |

`tamias config --file <path>` overrides discovery for that invocation;
without it, `tamias config` reads the first of `./tamias.yml`,
`./tamias.yaml`, `./tamias.toml` it finds.

## Exit codes

A failed verb exits non-zero. `verify` fails on digest mismatch;
`doctor` fails if any check fails (the report still prints first).
`cache clean --all` and `--days` conflict — pick one.
