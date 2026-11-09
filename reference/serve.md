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
| `install` | `ref`, `allow_drivers?` | `{label, resolved, digest}` |
| `list` | — | `[{label, source_ref, digest}]` |
| `info` | `name` | `{label, resolved, digest, size}` |
| `verify` | `name` | `{name, ok}` |
| `remove` | `name` | `{name}` — runs driver teardown first, like `tamias uninstall` |

`name` accepts `@scope/name` or bare `name`, same as the CLI.

## Driver consent

`install` refuses payload-shipped `drivers/` unless the request carries
`"allow_drivers": true` (or `TAMIAS_ALLOW_DRIVERS=1` is set) — identical
to `--allow-drivers`. Bundled drivers are never registered or executed
without it.

## Environment

The service honors the same roots as the CLI: `TAMIAS_HOME` (store +
`drivers/`), `TAMIAS_CACHE`, `TAMIAS_REGISTRY`, and the telemetry vars
(`TAMIAS_OTEL_*`).

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
