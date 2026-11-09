# How to manage project dependencies

`tamias install` fetches into the store. **`tamias add`** does that *and*
records the ref in your project's `tamias.yaml` (`deps:`) plus a resolved
entry in `tamias.lock` — run it from the project dir, not inside a
package's own source dir (a package's `tamias.yaml` is its package
manifest, a different document).

## Add and lock

```sh
tamias add git:9aia/peregrin//libraries/ci-gate#main
tamias add ./local-bundle --allow-drivers   # if it ships drivers/
```

This writes:

- `tamias.yaml` — `deps:` gains the ref (idempotent).
- `tamias.lock` — the resolved record: source ref, resolved sha,
  payload digest.

## Inspect

```sh
tamias list --json
tamias info ci-gate --json
tamias verify ci-gate --json
```

## Update and repin

```sh
tamias update ci-gate     # re-resolve + reinstall from the recorded ref
tamias update --all       # every installed package
tamias pin ci-gate git:9aia/peregrin//libraries/ci-gate#main~1
```

`update` and `pin` keep `tamias.lock` honest: if a lockfile exists in
the cwd and already pins the package, its entry is refreshed to the new
record. A missing lockfile — or one that doesn't pin the package — is
left alone (`add` is what creates it).

`update` re-runs the install path, so a package that ships `drivers/`
needs `--allow-drivers` (or `TAMIAS_ALLOW_DRIVERS=1`) again.

## Remove

```sh
tamias remove ci-gate    # drop the store entry; the fetch cache stays
```

`remove` only drops the store entry — it does **not** run the driver's
`uninstall` teardown. For provider-side cleanup use
`tamias uninstall ci-gate` (see
[Provision and service](/how-to/provision-a-service)).

The cache is content-addressed — removing never deletes shared fetch
entries. `tamias cache clean` maintains it separately (`--days <n>`, or
`--all`; the two conflict).

## See also

- [Install from a git repo](/how-to/install-from-git)
- [Ref grammar](/reference/refs)
- [Scripting tamias](/reference/scripting)
