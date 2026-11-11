---
url: /tamias/reference/serve.md
---
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
| `install` | `ref`, `allow_drivers?` | `{label, resolved, digest, signed}` |
| `plan` | `ref`, `allow_drivers?` | the `tamias install --plan` report — manifest, cache hit, payload digest, classified `include:`s, bundled drivers + consent need, kind driver, provider services, `trust`/`signer`/`signature_status`, `problems`. Dry-run: nothing is committed or dispatched |
| `sync` | `locked?`, `from_lock?`, `allow_drivers?`, `dir?` | `{deps: [{dep, status, detail}], failed}` — reconciles the project manifest's `deps:` against the store + `tamias.lock` (`dir` defaults to the server's cwd; `from_lock: true` makes `tamias.lock` itself the spec, like `tamias sync --from-lock`) |
| `list` | — | `[{label, source_ref, digest}]` |
| `info` | `name` | `{label, resolved, digest, size}` |
| `verify` | `name` | `{name, ok, signature, trust, signer}` — `trust`/`signer` are re-derived live, same rule as `tamias verify` |
| `remove` | `name` | `{name}` — runs driver teardown first, like `tamias uninstall` |
| `search` | `query`, `registry?` | `{results: [{label, name, scope, version, kind, digest, signed, publisher, file}]}` |
| `publish` | `dir`, `registry?`, `keep_includes?`, `keep?`, `sign?` | `{label, name, scope, version, kind, digest, signed, publisher, file, pruned}` — `keep` is the per-invocation retention override (P-024), `sign: true` attaches an ssh signature (`publish --sign`) |
| `registry.list` | — | `{registries: [{name, id, kind, effective_kind, url, token_env, limits, retention}]}` |
| `registry.get` | `name` | one registry object (same shape as `registry.list` items) |
| `registry.add` | `name`, `kind`, `url`, `token_env?`, `id?` | `{name, config}` — writes the config file |
| `registry.remove` | `name` | `{name, config}` |

`name` accepts `@scope/name` or bare `name`, same as the CLI.

`install` does **not** deploy `bins[]` — bin deployment is a
CLI-side post-commit step (`tamias install`'s default pass /
`tamias bin deploy`) that the serve ops don't run. A serve-driven
install of a bins\[] pack leaves the store entry committed but the
commands undeployed until something calls `tamias bin deploy <pkg>`.
(The `sync` op shares this — and so does the CLI's own `tamias sync`,
which installs through `tamias-install` without the bin pass.)

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
without it; once consented, the grant is remembered per package +
payload digest in `consented.json` (see
[Security model](/explanation/security)).

## Trust boundary

**Stdin is fully trusted.** Any process that can write to the service's
stdin can pass `"allow_drivers": true` and reach every op — there is no
per-request authentication. Embed `tamias serve` only behind a harness
that has already made the trust decision; never expose it to untrusted
input.

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

* [Scripting tamias](/reference/scripting) — `--json`, `manifest`, `schema`
* [Security model](/explanation/security)
