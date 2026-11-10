# How to put a package's binaries on your PATH

A package payload may ship a `bin/` directory of executables — release
tarballs installed from `https://…#sha256:…` refs use this layout (see
the binary-distribution ADR). `install` unpacks the payload into the
store; **`tamias bin`** is the verb that makes those executables
runnable by bare name — for your shell and for provider
`services[].exec`.

## Link a package's binaries

```sh
tamias install "https://github.com/9aia/tamias/releases/download/v0.1.2/tamias-x86_64-unknown-linux-gnu.tar.gz#sha256:…"
tamias bin tamias
```

`tamias bin <name>` accepts `@scope/name` or a bare `name`, and links
every regular file directly under the package's `bin/` (dotfiles and
nested dirs are skipped) into the bin dir:

- `$TAMIAS_BIN_DIR`, else `$XDG_BIN_HOME`, else `~/.local/bin`.

Each link prints as it is made; `--json` emits the same data as JSON.

## Why links survive upgrades

Store entry dirs are versioned (`name@<version>` or a sha label) and a
new version installs to a *new* dir — a link aimed at the raw entry dir
would dangle after `tamias update`. Instead `tamias bin` keeps a stable
per-name **alias** — `bin-alias/<name>` as a sibling of the store root
(`~/.local/share/tamias/bin-alias` by default, `$TAMIAS_HOME/bin-alias`
under `TAMIAS_HOME`) pointing at the current entry dir — and links
`<bin>/<file>` → `<alias>/bin/<file>`. Installs and updates retarget
the alias automatically when it exists, so existing links keep
resolving. Re-run `tamias bin <name>` after an upgrade that *adds* or
*renames* binaries — the alias follows, but new payload files need a
fresh link pass.

On Unix the links are symlinks. On Windows — where symlink creation
needs privileges — the payload file is **copied** instead; re-run
`tamias bin` after upgrades to refresh stale copies (`--force` also
works).

## Conflicts and removal

`tamias bin` refuses to overwrite anything already at a link path —
a foreign file, a foreign symlink, even a directory — unless `--force`
is passed. A link `tamias bin` already made is idempotent: re-running
reports `already linked` and changes nothing.

```sh
tamias bin tamias --unlink   # remove this package's links + alias
tamias bin tamias --force    # take over conflicting paths
```

`--unlink` removes the bin-dir entries the package owns — links into
its alias, plus byte-identical copies on copy platforms — and drops
the alias itself. It never touches files that don't belong to the
package. `tamias remove` and `tamias uninstall` run the same cleanup
automatically once the store entry is dropped, so links don't dangle;
`--unlink` remains the verb for unlinking while the package stays
installed. Provider services that named the bare binary keep working
until the links go away; after that, update their `exec:` to a full
path or reinstall the binaries.

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
bytes; a moving ref (branch, `latest`) re-resolves. `--allow-drivers`
and `--json` behave as they do for `install`/`update`. On Windows —
where `tamias bin` copies rather than symlinks — the running exe file
can be locked; self-upgrade warns before attempting and you may need
to close other tamias processes and retry.

## Errors you'll see

- `package 'x' is not installed` — install it first (`tamias install`).
- `<label>: no bin/ payload — nothing to link` — the package ships no
  `bin/` directory (or it's empty); nothing to do.
- `<path> already exists — refusing to overwrite (use --force)` —
  something else owns the link path.
- `… is not a tamias-managed binary` (`self-upgrade`) — the running
  exe wasn't installed via tamias; use your package manager or
  `cargo install` instead.
