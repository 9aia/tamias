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
| `id` | `string` | optional | Stable identifier (typically a uuid) — registry-scoped install refs (`&lt;label&gt;:&lt;name&gt;@&lt;version&gt;`) match `id` before the map key (`name`), so an entry can be renamed without breaking refs that pin the id. Must be ref-label-safe (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`) to be usable in a ref. |
| `kind` | `string` | optional | Explicit kind — `local` | `http` | `cf`. Unset → inferred from `url` (http(s) → `http`, anything else → `local`), so configs written before kinds were explicit keep working. |
| `url` | `string` | optional | Base URL (http) or root dir. |
| `token_env` | `string` | optional | Env var **name** holding the bearer token — never the token. |
| `limits` | `Limits` | optional | Per-registry limit overrides (values + package rules). |
| `retention` | `Retention` | optional | Per-registry retention overrides — how many of a package's most recent versions stay indexed after publish (P-024). |

## `[publish]` — publish routing

| field | type | required | notes |
|---|---|---|---|
| `default` | `string` | optional | Registry name `publish` targets by default. |

## `[signing]` — package signing policy

| field | type | required | notes |
|---|---|---|---|
| `key` | `string` | optional | Private key path for `publish --sign` (`~` expands) — used when `$TAMIAS_SIGNING_KEY` and `key_env`'s var are unset. |
| `key_env` | `string` | optional | Env var **name** whose value is the signing-key path — checked after `$TAMIAS_SIGNING_KEY`, before `key`. |
| `allowed_signers` | `string` | optional | `allowed_signers` file (authorized_keys-style, principal = `@scope/name` for scoped packages, else the bare name — with a bare-name fallback) for signature verification. `$TAMIAS_ALLOWED_SIGNERS` wins; default `~/.config/tamias/allowed_signers`. |
| `official_signers` | `string` | optional | `official_signers` file — the smaller trust set backing the official-badge check. `$TAMIAS_OFFICIAL_SIGNERS` wins; default `~/.config/tamias/official_signers`. |
| `required` | `bool` | optional | Every install must carry a valid signature. Only registry (`cf:`) refs can supply one — see `tamias-sign`'s docs. |
| `namespace` | `string` | optional | `ssh-keygen -Y` namespace (default `tamias-package`). |

## `[retention]` — publish retention (P-024)

| field | type | required | notes |
|---|---|---|---|
| `keep` | `int` | optional | Versions kept per package — `0` keeps only the just-published version. `None` inherits the next level / disables pruning. |
| `packages` | `BTreeMap<String, Retention>` | required | Per-package overrides — exact name or `*` glob → retention. Lives on the global `retention` (and may appear under a registry's `retention`); `packages` nested inside a package entry is ignored — overrides are one level deep. |

## Top level

| field | type | required | notes |
|---|---|---|---|
| `store` | `Location` | optional | Install store location — `root` applies when `kind` is `local`/unset and `TAMIAS_HOME`/XDG env is unset (env wins). |
| `cache` | `Location` | optional | Fetch-cache location — same resolution rule as `store`. |
| `registries` | `BTreeMap<String, Registry>` | optional | Named registries — `publish`/`search --registry &lt;name&gt;` and the `publish.default`/`search.default` routing resolve through these; registry-scoped install refs (`&lt;label&gt;:&lt;name&gt;@&lt;version&gt;`) match an entry's `id` first, then its map key. |
| `publish` | `Publish` | optional | Publish routing. |
| `search` | `Search` | optional | Search routing. |
| `limits` | `Limits` | optional | Global byte limits + per-package overrides. |
| `retention` | `Retention` | optional | Global retention policy — versions kept per package after publish (P-024). Unset → no pruning. |
| `signing` | `Signing` | optional | Package signing policy — `publish --sign` key resolution and install/verify `allowed_signers`/`official_signers` enforcement and trust-tier reporting (`tamias-sign`). |

## format trait (parse/render)

| field | type | required | notes |
|---|---|---|---|

## Formats

YAML is the standard — `tamias.yml` or `tamias.yaml`; `tamias.toml`
parses into the same `Config`. Read-side resolution is **layered**:
the user-level `~/.config/tamias/tamias.*` (`$XDG_CONFIG_HOME`
wins) is the base and a `./tamias.{yml,yaml,toml}` in the cwd
overlays it per-field — project wins, global fills the gaps.
`--file`/`TAMIAS_CONFIG` replaces discovery with that one file.
Registry *writes* (`tamias registry add|remove`) stay file-scoped:
they edit the project `tamias.*` (or `--file` target), never the
user layer implicitly. The merged config informs `doctor`'s
registry check (`publish.default` → `registries.NAME`).

## Constraints

* Token values never live in the file — `token_env` fields name
  the env var that carries them (the http client reads
  `TAMIAS_REGISTRY_TOKEN`).
* `tamias config --json` dumps the effective config;
  `tamias schema` emits the JSON Schema for the *package* manifest
  (`tamias.yaml`), a different document.
