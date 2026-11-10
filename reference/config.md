---
url: /tamias/reference/config.md
---
# Config — `tamias.yml` / `tamias.yaml` / `tamias.toml`

## `store` / `cache` — an install/fetch location

| field | type | required | notes |
|---|---|---|---|
| `kind` | `string` | optional | Impl kind — `local` (default) or `cf`. |
| `root` | `string` | optional | Root dir (or URL base for `cf`). |

## `registries.NAME` — a named registry

| field | type | required | notes |
|---|---|---|---|
| `kind` | `string` | optional | Explicit kind — `local` | `http` | `cf`. Unset → inferred from `url` (http(s) → `http`, anything else → `local`), so configs written before kinds were explicit keep working. |
| `url` | `string` | optional | Base URL (http) or root dir. |
| `token_env` | `string` | optional | Env var **name** holding the bearer token — never the token. |
| `limits` | `Limits` | optional | Per-registry limit overrides (values + package rules). |

## `[publish]` — publish routing

| field | type | required | notes |
|---|---|---|---|
| `default` | `string` | optional | Registry name `publish` targets by default. |

## Top level

| field | type | required | notes |
|---|---|---|---|
| `store` | `Location` | optional | Install store location — `root` applies when `kind` is `local`/unset and `TAMIAS_HOME`/XDG env is unset (env wins). |
| `cache` | `Location` | optional | Fetch-cache location — same resolution rule as `store`. |
| `registries` | `BTreeMap<String, Registry>` | optional | Named registries — `publish`/`search --registry &lt;name&gt;` and the `publish.default`/`search.default` routing resolve through these. |
| `publish` | `Publish` | optional | Publish routing. |
| `search` | `Search` | optional | Search routing. |
| `limits` | `Limits` | optional | Global byte limits + per-package overrides. |
| `signing` | `Signing` | optional | Package signing policy — `publish --sign` key resolution and install/verify `allowed_signers` enforcement (`tamias-sign`). |

## format trait (parse/render)

| field | type | required | notes |
|---|---|---|---|

## Formats

YAML is the standard — `tamias.yml` or `tamias.yaml`; `tamias.toml`
parses into the same `Config`. `tamias config` reads the first of
`./tamias.yml`, `./tamias.yaml`, `./tamias.toml` in the cwd
(`--file` overrides); no file means defaults. The same file informs
`doctor`'s registry check (`publish.default` → `registries.NAME`).

## Constraints

* Token values never live in the file — `token_env` fields name
  the env var that carries them (the http client reads
  `TAMIAS_REGISTRY_TOKEN`).
* `tamias config --json` dumps the effective config;
  `tamias schema` emits the JSON Schema for the *package* manifest
  (`tamias.yaml`), a different document.
