---
url: /tamias/how-to/link-binaries.md
---
# How to put a package's binaries on your PATH

A package declares its executable surfaces in the manifest — `bins[]`
entries — or ships a `bin/` payload dir of executables (the layout
release tarballs use). `install` deploys them into the bin dir **by
default**; `tamias bin` is the verb group for inspecting and managing
them explicitly.

## Declare `bins[]` in the manifest

```yaml
name: mycli
version: 1.0.0
type: mycli
bins:
  - name: mycli              # command name on PATH
    shim: cli                # generated front-end
    driver: mycli            # dispatches to tamias-driver-mycli's `cli` verb
  - name: mytool
    payload: tools/bin/x86_64-unknown-linux-gnu/mytool
```

Two declaration kinds (exactly one of `shim`/`payload` per entry):

* **`shim: cli` + `driver: <kind>`** — tamias writes a tiny shell
  script into the bin dir that execs `tamias bin exec <pkg> <name>
  "$@"`. `tamias` itself is the helper binary, so driver resolution
  stays dynamic — the shim follows `tamias-driver-<kind>` on `PATH` or
  in `<tamias-home>/drivers/` (including drivers the package itself
  just installed). This is how product CLIs ship: the `peregrin`
  pack's `bins: [{name: peregrin, shim: cli, driver: peregrin}]`.
* **`payload: tools/bin/<triple>/<file>`** — a real binary in the
  payload. Only the host triple's file deploys; declaring any triple's
  path resolves to the host's dir at deploy time, and a payload whose
  `tools/bin/` ships platform dirs but none for this host **fails the
  install**. A path without a triple dir (`tools/bin/<file>`) is a
  portable payload bin.

A package may also ship an undeclared `bin/` dir — every regular file
directly under it is linked too (legacy surface, same alias
machinery).

## Deploy, inspect, remove

```sh
tamias install ./mycli            # bins[] deploy automatically
tamias install ./mycli --no-bins  # opt out

tamias bin list                   # every package's bins + deploy state,
                                  # collisions flagged (--json too)
tamias bin deploy mycli           # materialize now (the retry path)
tamias bin deploy mycli --force   # take over conflicting bin paths
tamias bin destroy mycli          # remove this package's bins; pack stays
```

The bin dir is `$TAMIAS_BIN_DIR` → `$XDG_BIN_HOME` → `~/.local/bin`.
Name collisions **warn and last-deploy wins** — never a hard error —
and `bin list` shows which package owns each name. `destroy` (and
`remove`/`uninstall` after dropping the store entry) removes only
artifacts the package owns: shim scripts carry a `# tamias-bin: <label>` marker, payload bins are proven by their link target;
`--force` also clears foreign files at the bin paths.

## Why links survive upgrades

Store entry dirs are versioned (`name@<version>` or a sha label) and a
new version installs to a *new* dir — a link aimed at the raw entry dir
would dangle after `tamias update`. Instead payload bins link through a
stable per-package **alias** — `bin-alias/<name>` as a sibling of the
store root (`$TAMIAS_HOME/bin-alias`) pointing at the current entry
dir — so `<bin>/<file>` → `<alias>/…` keeps resolving across upgrades;
installs and updates retarget the alias automatically. Re-run
`tamias bin deploy` after an upgrade that *adds* or *renames* binaries
— the alias follows, but new payload files need a fresh deploy pass.

On Unix the links are symlinks. On Windows — where symlink creation
needs privileges — the payload file is **copied** instead; re-run
`tamias bin deploy` after upgrades to refresh stale copies (`--force`
also works).

## Upgrading the binary you're running

`tamias update <name>` upgrades a package, but the *running*
executable can't replace itself — a process keeps its old inode even
after the store entry swaps. **`tamias self-upgrade`** closes that
loop for tamias-managed binaries: it detects that the running exe
resolves into the store (directly, or through the `bin-alias`/bin-dir
link chain — a cargo or distro install fails with "not installed via
tamias"), then re-installs the package's recorded source ref, exactly
like `tamias update`. The install retargets the alias, so the *next*
invocation runs the new version; this process keeps the old one —
restart to pick it up.

```sh
tamias self-upgrade --check   # current vs what the ref resolves to
                              # — no install, fetches to the cache only
tamias self-upgrade           # update + retarget the alias
```

A release install pinned to a `#sha256:` ref re-fetches exactly those
bytes; a moving ref (branch, `latest`) re-resolves. `--allow-drivers`,
`--no-bins`, and `--json` behave as they do for `install`/`update`. On
Windows — where payload bins are copied rather than symlinked — the
running exe file can be locked; self-upgrade warns before attempting
and you may need to close other tamias processes and retry.

## Producing the tamias self-package

The tarball a self-upgrade consumes is built by the repo itself:

```sh
cargo xtask pack-self          # release-build → target/pack-self/tamias-<ver>.tgz
cargo xtask pack-self --skip-build --out /tmp/pkg   # reuse target/release/tamias
```

The verb release-builds `tamias`, stages a `type: files` package
(`tamias.yaml` + `bin/tamias`, version read from
`crates/tamias-cli/Cargo.toml`), and packs it through the same
`tamias-publish` path as `tamias bundle pack`. It prints the tgz's
sha256 and a ready-to-paste pinned install ref — host the tgz
somewhere (e.g. `tamias.9aia.com`) or drop it into a registry payload
dir for `tamias publish`.

## Errors you'll see

* `package 'x' is not installed` — install it first (`tamias install`).
* `<label>: nothing to deploy — no bins[] and no bin/ payload` — the
  package ships no executable surfaces; nothing to do.
* `<path> already exists — refusing to overwrite (use --force)` —
  something else owns the bin path. `bin list` shows the current owner.
* `… is not a tamias-managed binary` (`self-upgrade`) — the running
  exe wasn't installed via tamias; use your package manager or
  `cargo install` instead.
