# Config

> Generated from config-struct doc comments by `gen-config-docs.py`
> (`cargo xtask site`). Hand summary below until first generation.

`tamias.toml` — searched at `--config` → `$TAMIAS_CONFIG` →
`./tamias.toml` → `~/.config/tamias/tamias.toml`. Every section is
optional; zero-config uses the local store + local cache.

```toml
[store]
kind = "local"                 # local larder (v1)

[cache]
kind = "local"                 # content-addressed fetch cache

[registries.local]
kind = "dir"

# [registries.cloud]
# kind = "cf"
# url = "https://tamias-registry.you.workers.dev"
# token_env = "TAMIAS_CF_TOKEN"  # env var NAME — never the token
```

See [examples/tamias.toml.example](https://github.com/9aia/tamias/blob/main/examples/tamias.toml.example).
