# Config

Config lives in a `tamias` config file. **YAML is the standard**
(`tamias.yml` / `tamias.yaml`); **TOML** (`tamias.toml`) is the supported
alternative — both parse into the same model.

Search order: `--file` → `$TAMIAS_CONFIG` → `./tamias.yml` →
`./tamias.yaml` → `./tamias.toml` → `~/.config/tamias/tamias.yml`. Every
section is optional; zero-config uses the local store + local cache.

```yaml
# tamias.yml — the standard format
store:
  kind: local                 # local larder (v1)

cache:
  kind: local                 # content-addressed fetch cache

registries:
  local:
    kind: dir

  # cloud:
  #   kind: cf
  #   url: https://tamias-registry.you.workers.dev
  #   token_env: TAMIAS_CF_TOKEN   # env var NAME — never the token
```

The TOML alternative is identical in shape:

```toml
[store]
kind = "local"

[cache]
kind = "local"

[registries.local]
kind = "dir"
```

Inspect the effective config with `tamias config` (`--file <path>`,
`--json`). See
[examples/tamias.yml.example](https://github.com/9aia/tamias/blob/main/examples/tamias.yml.example)
and the TOML alternative
[examples/tamias.toml.example](https://github.com/9aia/tamias/blob/main/examples/tamias.toml.example).
