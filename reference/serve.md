# `tamias serve` — JSON-RPC over stdio

`tamias serve` runs tamias as an **ndjson JSON-RPC service**: one JSON
request per line on stdin, one JSON response per line on stdout. It's
the embedding surface — a parent process (an agent harness, a daemon)
drives installs without shelling out per verb.

## Framing

Request:  `{"op": "<op>", …fields}`
Response: `{"ok": true, "result": …}` or `{"ok": false, "error": "…"}`

A failed op returns `ok:false` — it does not kill the service. The
service exits when stdin closes.

## Ops

| op | fields | result |
| --- | --- | --- |
| `install` | `ref`, `allow_drivers?` | `{label, resolved, digest}` |
| `list` | — | `[{label, source_ref, digest}]` |
| `info` | `name` | `{label, resolved, digest, size}` |
| `verify` | `name` | `{name, ok}` |
| `remove` | `name` | `{name}` — runs driver teardown first, like `tamias uninstall` |
| `search` | `query`, `registry?` | `{results: [{label, name, scope, version, kind, digest, file}]}` |
| `publish` | `dir`, `registry?` | `{label, name, scope, version, kind, digest, file}` |
| `registry.list` | — | `{registries: [{name, kind, effective_kind, url, token_env, limits}]}` |
| `registry.get` | `name` | one registry object (same shape as `registry.list` items) |
| `registry.add` | `name`, `kind`, `url`, `token_env?` | `{name, config}` — writes the config file |
| `registry.remove` | `name` | `{name, config}` |

`name` accepts `@scope/name` or bare `name`, same as the CLI.

`search`/`publish` take `registry` as a **name or raw ref** — identical
resolution to `--registry` on the CLI verbs: config `registries` name →
raw dir/http ref → routing defaults (`search.default` →
`publish.default`) → `TAMIAS_REGISTRY` env → the local default root.
`registry.add`/`remove` write `tamias.{yml,yaml,toml}` through the same
path as `tamias registry add|remove`; `token_env` is an env var *name*,
never a token value.

## Driver consent

`install` refuses payload-shipped `drivers/` unless the request carries
`"allow_drivers": true` (or `TAMIAS_ALLOW_DRIVERS=1` is set) — identical
to `--allow-drivers`. Bundled drivers are never registered or executed
without it.

## Environment

The service honors the same roots as the CLI: `TAMIAS_HOME` (store +
`drivers/`), `TAMIAS_CACHE`, `TAMIAS_REGISTRY`, `TAMIAS_CONFIG` (the
config file `registry.*` ops read/write and `search`/`publish` resolve
against — re-read per request, so writes are visible in-session), and
the telemetry vars (`TAMIAS_OTEL_*`).

## Example

```sh
printf '%s\n' \
  '{"op":"install","ref":"./hello"}' \
  '{"op":"list"}' \
  '{"op":"verify","name":"hello"}' \
| tamias serve
```

## See also

- [Scripting tamias](/reference/scripting) — `--json`, `manifest`, `schema`
- [Security model](/explanation/security)
