---
url: /tamias/reference/config.md
---
# Config — `tamias.toml`

## `[locations.NAME]` — an install/fetch location

| field | type | required | notes |
|---|---|---|---|
| `kind` | `string` | optional | Impl kind — `local` (default) or `cf`. |
| `root` | `string` | optional | Root dir (or URL base for `cf`). |

## `[registries.NAME]` — a content registry

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
| `store` | `Location` | required | Install store location. |
| `cache` | `Location` | required | Fetch-cache location. |
| `registries` | `BTreeMap<String, Registry>` | required | Named registries. |
| `publish` | `Publish` | required | Publish routing. |

## format trait (parse/render)

| field | type | required | notes |
|---|---|---|---|

## `[envs]` route forms

Either a bare vault name or the table above:

```toml
[envs]
dev = "local"
prod = { vault = "cloud" }
```

## Constraints

* `deny_unknown_fields`: typos in field names fail the load.
* Search order: `--config` → `$TAMIAS_CONFIG` → `./tamias.toml` →
  `~/.config/tamias/tamias.toml`. The file is optional — no
  `tamias.toml` anywhere just means defaults (local age vault,
  `dev` env).
* Token values never live in the file — `token_env` fields name
  the env var that carries them.
* `tamias schema` emits the JSON Schema for this shape;
  `tamias check` validates a real file end-to-end.
