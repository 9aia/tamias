# Tutorial: your first package

A guided walk from a folder to a published, installed, verified package.

## 1. Make a payload

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

## 2. Pack it

```sh
tamias bundle pack ./hello
# → packed → hello-0.1.0.tgz
```

## 3. Publish it

Serve a registry (any directory works) and publish into it:

```sh
tamias registry serve --addr 127.0.0.1:8788 --dir ~/.local/share/tamias/registry &
tamias publish ./hello --registry http://127.0.0.1:8788
# → published hello@0.1.0
tamias search hello --registry http://127.0.0.1:8788
```

## 4. Install it

```sh
tamias install ./hello
tamias list
tamias info hello
```

`install` resolved the ref, fetched it, cached the payload by content, and
unpacked it into the **install store** under `~/.local/share/tamias`.

## 5. Verify it

```sh
tamias verify hello
# → hello: ok (sha256:…)
```

`verify` recomputes the payload digest and compares it to the recorded
one. Now poke the store and verify again — it should fail:

```sh
echo tampered >> ~/.local/share/tamias/hello@0.1.0/world/greeting.txt
tamias verify hello   # → digest mismatch
```

## 6. Untamper, then keep going

Reinstall to restore, then try a git ref:

```sh
tamias install git:9aia/peregrin//libraries/ci-gate#v0.2.0
tamias list --type peregrin-bundle
```

## Next

- [How-to guides](/how-to/publish-to-a-registry)
- [Ref grammar](/reference/refs)
- [CLI reference](/reference/cli)
