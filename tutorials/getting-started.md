# Getting Started

Your first package, end to end: make a payload, publish it, install it
back, and prove the bytes are intact. By the end you'll have a package
living in tamias's install store — fetched, cached, and digest-verified.

## 1. Install tamias

```sh
cargo install tamias
# or from source:
cargo install --git https://github.com/9aia/tamias tamias
# or, from a checkout:
cargo xtask install
```

`tamias` is a native binary — it stays `cargo install`-able because
every `bins[]` shim in the org execs `tamias bin exec` (bootstrap).
Product CLIs (`myco`, `cuora`, `kuda`, …) are *not* cargo-installable:
they ship as tamias packs — `tamias install 9aia:<name>@<version>`.

## 2. Make a payload

A package is *bytes + a manifest*. Any folder with a `tamias.yaml` is one.

```sh
mkdir -p hello/world
cat > hello/tamias.yaml <<'YAML'
name: hello
version: 0.1.0
type: files
YAML
echo "hi from hello" > hello/world/greeting.txt
```

`type: files` means "the store entry is the whole story" — no driver.

## 3. Pack and publish it

Serve a registry (any directory works), pack, and publish into it:

```sh
tamias registry serve --addr 127.0.0.1:8788 --dir ~/.local/share/tamias/registry &
tamias bundle pack ./hello
# → packed → hello-0.1.0.tgz
tamias publish ./hello --registry http://127.0.0.1:8788
# → published hello@0.1.0
tamias search hello --registry http://127.0.0.1:8788
```

## 4. Install it

```sh
tamias install ./hello
tamias list
tamias info hello
tamias install 9aia:ci-gate@0.2.0    # named registry — <registry>:<pkg>@<ver>
```

`install` resolved the ref, fetched it, cached the payload by content, and
unpacked it into the **install store** under `~/.local/share/tamias`.
`info` shows provenance: source ref, resolved sha, digest.

## 5. Verify it — then break it

```sh
tamias verify hello
# → hello: ok (sha256:…)
tamias doctor   # store, cache, git, registry, drivers — all green?
```

`verify` recomputes the payload digest and compares it to the recorded
one. Now poke the store and verify again — it should fail:

```sh
echo tampered >> ~/.local/share/tamias/hello@0.1.0/world/greeting.txt
tamias verify hello   # → digest mismatch
```

Reinstall to restore. Then try a git ref — installs resolve straight from
a repo subdir at a rev:

```sh
tamias install git:9aia/bupha//libraries/ci-gate#main
tamias info ci-gate
```

## What you now know

- A package is `bytes + a manifest`; `tamias.yaml` declares name,
  version, and `type`.
- `bundle pack` → `.tgz`; `publish` writes it to a registry index;
  `install` fetches by ref and commits to the content-addressed store.
- Every install records source ref, resolved sha, and payload digest —
  `verify` recomputes it, and tampering is detected.
- Refs are one grammar: paths, git (`git:org/repo//subdir#rev`),
  tarballs over HTTP, `cf:` and named registries.

## Next

- [How-to guides](/how-to/publish-to-a-registry)
- [Manage project dependencies](/how-to/manage-deps) — `add` + `tamias.lock`
- [Ref grammar](/reference/refs)
- [CLI reference](/reference/cli)
- [Security model](/explanation/security) — digests, drivers, `--allow-drivers`
