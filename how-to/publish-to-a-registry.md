# How to publish to a registry

Publish a packed package into a registry — a local directory, or a
Cloudflare Worker (R2 blobs + D1/KV index).

## Local registry

```sh
tamias registry serve --addr 127.0.0.1:8788 --dir ~/.local/share/tamias/registry &
tamias publish ./hello --registry http://127.0.0.1:8788
tamias search hello --registry http://127.0.0.1:8788
```

Without `--registry`, `publish` writes to the default local directory.

## Consume it

```sh
tamias install ./hello                     # a path
tamias install https://host/hello-0.1.0.tgz#sha256:<hex>
```

Digest-pin the tarball (`#sha256:<hex>`) when you have the bytes.

## Cloudflare registry

1. Write the Worker + config:

   ```sh
   tamias deploy-registry --out ./tamias-registry-worker
   ```

2. Deploy the Worker with your Cloudflare account and bind R2 + D1/KV.

3. Point a registry entry at it. In `tamias.yml`:

   ```yaml
   registries:
     cloud:
       kind: cf
       url: https://tamias-registry.you.workers.dev
       token_env: TAMIAS_CF_TOKEN   # env var NAME — never the token
   ```

4. Publish + search through it:

   ```sh
   tamias publish ./hello --registry https://tamias-registry.you.workers.dev
   ```

The protocol is the same either way: the client does a read-modify-write
of `index.json`; the server is a token-guarded blob store (`GET`/`PUT
/<key>`). Auth is a bearer token from `token_env` — never inline.

## See also

- [Config reference](/reference/config)
- [Tutorial: your first package](/tutorials/first-package)
