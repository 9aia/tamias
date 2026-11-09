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
| `kind` | `string` | optional | `local` (default) or `cf`. |
| `url` | `string` | optional | Base URL (http) or root dir. |
| `token_env` | `string` | optional | Env var **name** holding the bearer token — never the token. |

## `[publish]` — publish routing

| field | type | required | notes |
|---|---|---|---|
| `default` | `string` | optional | Registry name `publish` targets by default. |

## Top level

| field | type | required | notes |
|---|---|---|---|
| `store` | `Location` | optional | Install store location. |
| `cache` | `Location` | optional | Fetch-cache location. |
| `registries` | `BTreeMap<String, Registry>` | optional | Named registries. |
| `publish` | `Publish` | optional | Publish routing. |

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
