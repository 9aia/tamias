---
url: /tamias/how-to/manage-deps.md
---
# How to manage project dependencies

`tamias install` fetches into the store. **`tamias add`** does that *and*
records the ref in your project's `tamias.yaml` (`deps:`) plus a resolved
entry in `tamias.lock` — run it from the project dir, not inside a
package's own source dir (a package's `tamias.yaml` is its package
manifest, a different document).

## Add and lock

```sh
tamias add git:9aia/bupha//libraries/ci-gate#main
tamias add ./local-bundle --allow-drivers   # if it ships drivers/
```

This writes:

* `tamias.yaml` — `deps:` gains the ref (idempotent).
* `tamias.lock` — the resolved record: source ref, resolved sha,
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
tamias pin ci-gate git:9aia/bupha//libraries/ci-gate#main~1
```

`update` and `pin` keep `tamias.lock` honest: if a lockfile exists in
the cwd and already pins the package, its entry is refreshed to the new
record. A missing lockfile — or one that doesn't pin the package — is
left alone (`add` is what creates it).

`update` re-runs the install path, so a package that ships `drivers/`
needs `--allow-drivers` (or `TAMIAS_ALLOW_DRIVERS=1`) again.

## Sync a fresh checkout

`deps:` + `tamias.lock` are a write-only ledger until you reconcile
them — **`tamias sync`** installs the project's whole dep set, the
missing half of the dependency story:

```sh
tamias sync          # install what's missing or drifted
tamias sync --json   # per-dep outcomes as JSON
tamias sync --locked # strict: refuse to move anything the lock pins
```

For each `deps:` ref, sync checks the store entry against the lock
entry (`source_ref`/`resolved`/`digest`):

* **current** — the store record matches the lock; nothing is fetched.
* **installed / reinstalled** — the dep was missing or drifted, so the
  shared install pipeline runs and `tamias.lock` is refreshed (moving
  refs like `#branch` re-resolve, same as `add`).
* **failed** — the dep's install errored; sync keeps going and exits
  non-zero at the end.

Sync **never removes** store entries — the store is global, not
project-scoped, so extraneous packages are `tamias remove`'s job. A
dep that ships `drivers/` needs `--allow-drivers` (or
`TAMIAS_ALLOW_DRIVERS=1`) like `install`/`add`.

Note: `sync` installs through the shared pipeline but skips the
`bins[]` deploy pass that `install`/`add`/`update` run — a synced pack
with declared bins lands in the store without its commands on `PATH`.
Run `tamias bin deploy <pkg>` (or `tamias bin list` to see what's
missing) afterwards.

`--locked` is the reproducible-install mode: every dep must already be
`current` per `tamias.lock` + the store. Drift, a missing store entry,
or a dep with no lock entry fails that dep instead of installing or
moving it — the lockfile must already describe the world.

`tamias sync --from-lock` flips the spec: `tamias.lock` entries are
the source of truth — each recorded `source_ref` installs pinned, the
manifest's `deps:` are ignored, and the lock is never rewritten. Use it
for reproducible restores; use plain `sync` to move with your refs.

The same reconcile is available to embedders as the `sync` op of
`tamias serve` (`{"op":"sync","locked":…,"from_lock":…,"allow_drivers":…,"dir":…}` —
`dir` defaults to the server's cwd) and in the TUI's Manifest pane
(`s` or `:sync`, behind a confirm).

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

* [Install from a git repo](/how-to/install-from-git)
* [Ref grammar](/reference/refs)
* [Scripting tamias](/reference/scripting)
