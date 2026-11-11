# How to publish to a registry

Publish a packed package into a registry — a local directory, or a
Cloudflare Worker (R2 blobs + D1/KV index).

## Local registry

```sh
tamias registry serve --addr 127.0.0.1:8788 --dir ~/.local/share/tamias/registry &
tamias publish ./hello --registry http://127.0.0.1:8788
tamias search hello --registry http://127.0.0.1:8788
```

`registry serve` is an **open dev server** — anyone who can reach the
port can read and write the index. Set `TAMIAS_REGISTRY_SERVE_TOKEN`
before serving and clients must send `Authorization: Bearer <token>`
(the client's token comes from `TAMIAS_REGISTRY_TOKEN`).

Without `--registry`, `publish` writes to the default local directory.

## Consume it

```sh
tamias install ./hello                     # a path
tamias install https://host/hello-0.1.0.tgz#sha256:<hex>
```

Digest-pin the tarball (`#sha256:<hex>`) when you have the bytes.

With a **named registry** in config (`tamias registry add`, see below),
install by name — `<registry>:<package>@<version>`:

```sh
tamias install 9aia:hello@0.1.0
tamias install 9aia:@acme/scoped@2.0.0    # scoped package
```

The label matches the registry's `id` (`registry add --id`) first,
then its name; `@<version>` is required. The same refs work in a
project `tamias.yaml`'s `deps:` for `tamias sync`.

## Cloudflare registry

1. Write the Worker + config:

   ```sh
   tamias deploy-registry --out ./tamias-registry-worker
   ```

2. Deploy the Worker with your Cloudflare account and bind R2 + D1/KV.

3. Name the registry once — globally so every project sees it:

   ```sh
   tamias registry --file ~/.config/tamias/tamias.toml add 9aia \
     --kind cf --url https://tamias-registry.you.workers.dev \
     --token-env TAMIAS_REGISTRY_TOKEN
   ```

   (`--file` sits on `registry`, before the subcommand; inside a project
   drop it to write that project's `tamias.yml` instead — the user-level
   file is the base layer a project `tamias.yml` overlays per-field.)

4. Route publish/search at it by default — add to the same file:

   ```toml
   [publish]
   default = "9aia"

   [search]
   default = "9aia"
   ```

5. Publish + search through it — no `--registry` needed:

   ```sh
   export TAMIAS_REGISTRY_TOKEN=…   # the env var NAMED by token_env
   tamias publish ./hello
   tamias search hello
   ```

The protocol is the same either way: the client does a read-modify-write
of `index.json`; the server is a token-guarded blob store (`GET`/`PUT
/<key>`). Auth is a bearer token — the config `token_env` field names
the env var that carries it; clients honor `token_env` (falling back
to `TAMIAS_REGISTRY_TOKEN`). Never inline a token.

## Retention — pruning old versions

Every publish can prune the package's older **index** entries so a
registry doesn't accumulate versions forever (P-024). Configure it in
`tamias.yml`:

```yaml
retention:
  keep: 10                      # versions kept per package
  packages:                     # optional per-package overrides
    "big-*":
      keep: 50
```

`keep` counts versions in **semver order** (prereleases included —
`1.0.0-rc.1` sorts below `1.0.0`), not publish order, and applies per
`(scope, name)`. The version being published is never pruned, and
versions that aren't semver are never pruned either. A `retention:`
block under `registries.<name>` overrides the global policy for that
registry. `tamias publish --keep N` overrides per invocation, and
`tamias publish --json` reports the pruned labels.

Pruning is **index-entry only**: the `.tgz` payload blobs stay in the
registry, but a pruned version no longer resolves through the index —
`search` won't list it and `cf:<name>@<version>` installs can't resolve
it (the `cf:` source reads `index.json`). A direct fetch of the blob
URL still works, and installed copies on consumer machines are
untouched — `verify` recomputes the payload digest locally. Unset
`keep` = keep everything (the default).

## See also

- [Config reference](/reference/config)
- [Tutorial: your first package](/tutorials/first-package)
