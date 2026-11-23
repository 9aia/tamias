# Command-Line Help for `tamias`

This document contains the help content for the `tamias` command-line program.

Package & content manager — fetch, store, cache, dispense

**Command Overview:**

* [`tamias`↴](#tamias)
* [`tamias install`↴](#tamias-install)
* [`tamias add`↴](#tamias-add)
* [`tamias sync`↴](#tamias-sync)
* [`tamias remove`↴](#tamias-remove)
* [`tamias prune`↴](#tamias-prune)
* [`tamias list`↴](#tamias-list)
* [`tamias info`↴](#tamias-info)
* [`tamias verify`↴](#tamias-verify)
* [`tamias update`↴](#tamias-update)
* [`tamias pin`↴](#tamias-pin)
* [`tamias self-upgrade`↴](#tamias-self-upgrade)
* [`tamias bin`↴](#tamias-bin)
* [`tamias bin list`↴](#tamias-bin-list)
* [`tamias bin deploy`↴](#tamias-bin-deploy)
* [`tamias bin destroy`↴](#tamias-bin-destroy)
* [`tamias cache`↴](#tamias-cache)
* [`tamias cache clean`↴](#tamias-cache-clean)
* [`tamias version`↴](#tamias-version)
* [`tamias manifest`↴](#tamias-manifest)
* [`tamias schema`↴](#tamias-schema)
* [`tamias gen-cli-docs`↴](#tamias-gen-cli-docs)
* [`tamias completions`↴](#tamias-completions)
* [`tamias doctor`↴](#tamias-doctor)
* [`tamias config`↴](#tamias-config)
* [`tamias serve`↴](#tamias-serve)
* [`tamias publish`↴](#tamias-publish)
* [`tamias search`↴](#tamias-search)
* [`tamias bundle`↴](#tamias-bundle)
* [`tamias bundle pack`↴](#tamias-bundle-pack)
* [`tamias registry`↴](#tamias-registry)
* [`tamias registry serve`↴](#tamias-registry-serve)
* [`tamias registry add`↴](#tamias-registry-add)
* [`tamias registry remove`↴](#tamias-registry-remove)
* [`tamias registry get`↴](#tamias-registry-get)
* [`tamias registry list`↴](#tamias-registry-list)
* [`tamias deploy-registry`↴](#tamias-deploy-registry)
* [`tamias deploy`↴](#tamias-deploy)
* [`tamias uninstall`↴](#tamias-uninstall)
* [`tamias service`↴](#tamias-service)
* [`tamias service install`↴](#tamias-service-install)
* [`tamias service enable`↴](#tamias-service-enable)
* [`tamias service disable`↴](#tamias-service-disable)
* [`tamias service status`↴](#tamias-service-status)
* [`tamias service logs`↴](#tamias-service-logs)
* [`tamias service uninstall`↴](#tamias-service-uninstall)
* [`tamias drivers`↴](#tamias-drivers)
* [`tamias driver`↴](#tamias-driver)
* [`tamias driver config-schema`↴](#tamias-driver-config-schema)
* [`tamias provider`↴](#tamias-provider)
* [`tamias provider list`↴](#tamias-provider-list)
* [`tamias run`↴](#tamias-run)
* [`tamias logs`↴](#tamias-logs)
* [`tamias ui`↴](#tamias-ui)

## `tamias`

Package & content manager — fetch, store, cache, dispense

**Usage:** `tamias [COMMAND]`

###### **Commands:**

* `install` — Fetch → store a package from a ref
* `add` — Install a ref and record it in `tamias.yaml` + `tamias.lock`
* `sync` — Reconcile the cwd project's dep set against the store — install what's missing or drifted. Never removes store entries: the store is global, not project-scoped; `tamias remove` stays the uninstall verb
* `remove` — Drop a store entry (cache untouched)
* `prune` — Prune superseded store versions — **dry-run by default**
* `list` — List installed packages
* `info` — Show one package's manifest + provenance
* `verify` — Digest-check a package against its record
* `update` — Re-resolve + re-install a package from its recorded ref
* `pin` — Retarget an installed package's recorded source ref
* `self-upgrade` — Update the tamias package this binary belongs to
* `bin` — Manage a package's executable surfaces — the declared `bins[]` (generated `cli` shims + `tools/bin/<triple>/` payloads) and `bin/` payload files — on the bin dir (`$TAMIAS_BIN_DIR` → `XDG_BIN_HOME` → `~/.local/bin`)
* `cache` — Fetch-cache maintenance
* `version` — Print the tamias version
* `manifest` — Print the machine-readable capability manifest
* `schema` — Print the `tamias.yaml` JSON Schema
* `gen-cli-docs` — Print the CLI reference (markdown) generated from the parser
* `completions` — Print a shell completion script
* `doctor` — Diagnose the host metaservice (store, cache, registry, drivers)
* `config` — Show the effective config — YAML (`tamias.yml`) is the standard
* `serve` — Run the ndjson JSON-RPC service over stdio
* `publish` — Publish a package dir to a registry
* `search` — Search a registry's index
* `bundle` — Author-side packing
* `registry` — Registry operations — named sources in the config file
* `deploy-registry` — Deploy the registry Worker (Cloudflare)
* `deploy` — Deploy an installed package's service/runtime (via its provider)
* `uninstall` — Uninstall — provider teardown + drop the store entry
* `service` — Manage a per-user service unit (systemd)
* `drivers` — List discovered content drivers (`tamias-driver-<type>` on PATH + `<tamias-home>/drivers/`)
* `driver` — Provider-verb dispatch — run a driver verb beyond the lifecycle three (`install`/`deploy`/`uninstall`)
* `provider` — Provider packages — the declarative `provider:` runtime spec
* `run` — Run an installed provider service's exec in the foreground — the unit's argv + env, without systemd. `tamias run <pkg> [service] [-- extra-args]`
* `logs` — Follow a package's provider units' journals — `tamias logs <pkg|unit>`. A package name resolves to all its provider units; anything else is treated as a unit base name
* `ui` — Interactive package-manager TUI (ratatui) — installed entries, registry search, provider lifecycle, manifest deps, authoring

###### **Options:**

* `-h, --help` — Print help
* `-V, --version` — Print version

## `tamias install`

Fetch → store a package from a ref

**Usage:** `tamias install [OPTIONS] <REFERENCE>`

###### **Arguments:**

* `<REFERENCE>` — ./path | git:&lt;url&gt;[//subdir][#rev] | https://….tgz[#sha256:…]

###### **Options:**

* `--allow-drivers` — Consent to register + run the package's bundled `drivers/`. Equivalent: `TAMIAS_ALLOW_DRIVERS=1` — a global env bypass honored by every surface (CLI, serve, TUI)
* `--no-bins` — Don't deploy the package's declared `bins[]` into the bin dir (install deploys them by default)
* `--plan` — Dry-run — report everything the install would do (manifest, `include:` resolution, bundled drivers + consent, kind driver, provider services, trust/signature, would-be problems) without dispatching drivers or committing the store. The fetch still populates the content-addressed cache
* `--json` — JSON output (with `--plan`)
* `-h, --help` — Print help

## `tamias add`

Install a ref and record it in `tamias.yaml` + `tamias.lock`. Writes the *project* manifest (`deps:`/`include:`) — run from the project dir, not inside a package's source dir (a package's own `tamias.yaml` is its package manifest).

**Usage:** `tamias add [OPTIONS] <REFERENCE>`

###### **Arguments:**

* `<REFERENCE>` — The ref to add

###### **Options:**

* `--allow-drivers` — Consent to register + run the package's bundled `drivers/`. Equivalent: `TAMIAS_ALLOW_DRIVERS=1` — a global env bypass honored by every surface (CLI, serve, TUI)
* `--no-bins` — Don't deploy the package's declared `bins[]` into the bin dir (install deploys them by default)
* `-h, --help` — Print help (see a summary with '-h')

## `tamias sync`

Reconcile the cwd project's dep set against the store — install what's missing or drifted. Never removes store entries: the store is global, not project-scoped; `tamias remove` stays the uninstall verb. By default the spec is the project manifest's `deps:` (tamias.yaml + tamias.lock); `--from-lock` swaps the spec to `tamias.lock` itself — each entry's pinned `source_ref` is installed, the manifest is not consulted, and the lock is never rewritten.

**Usage:** `tamias sync [OPTIONS]`

###### **Options:**

* `--locked` — Strict mode — every dep must already be current per `tamias.lock` + the store; drift or a missing lock entry fails the dep instead of installing/moving it
* `--from-lock` — Install `tamias.lock` entries verbatim (the lock is the spec) instead of reconciling the manifest's `deps:`
* `--allow-drivers` — Consent to register + run the packages' bundled `drivers/`. Equivalent: `TAMIAS_ALLOW_DRIVERS=1` — a global env bypass honored by every surface (CLI, serve, TUI)
* `--json` — JSON output
* `-h, --help` — Print help (see a summary with '-h')

## `tamias remove`

Drop a store entry (cache untouched)

**Usage:** `tamias remove <NAME>`

###### **Arguments:**

* `<NAME>` — `@scope/name` or `name`

###### **Options:**

* `-h, --help` — Print help

## `tamias prune`

Prune superseded store versions — **dry-run by default**. Groups installed entries by package (`@scope/name`) and reports every version older than the newest `keep` semver releases — the store keeps every version ever installed, so this is the explicit garbage-collection verb. Nothing is removed without `--yes`; removals run through the same teardown path as `uninstall` (provider units re-rendered from survivors, bins cleanup, version-aware driver dispatch, store drop). Entries without a semver `version:` are never touched — they are reported as skipped instead. Never runs automatically on `install`/`update`/`sync`: old versions are rollback targets.

**Usage:** `tamias prune [OPTIONS]`

###### **Options:**

* `--keep <KEEP>` — Keep this many newest versions per package
* `-y, --yes` — Actually remove — without it this is a dry-run report
* `--json` — JSON output
* `-h, --help` — Print help (see a summary with '-h')

## `tamias list`

List installed packages

**Usage:** `tamias list [OPTIONS]`

###### **Options:**

* `--type <KIND>` — Only entries of this content type
* `--json` — JSON output
* `-h, --help` — Print help

## `tamias info`

Show one package's manifest + provenance

**Usage:** `tamias info [OPTIONS] <NAME>`

###### **Arguments:**

* `<NAME>` — `@scope/name` or `name`

###### **Options:**

* `--json` — JSON output
* `-h, --help` — Print help

## `tamias verify`

Digest-check a package against its record

**Usage:** `tamias verify [OPTIONS] <NAME>`

###### **Arguments:**

* `<NAME>` — `@scope/name` or `name`

###### **Options:**

* `--json` — JSON output
* `--require-official` — Fail unless the package's trust tier is `official` — the signature must verify against `signing.official_signers`
* `-h, --help` — Print help

## `tamias update`

Re-resolve + re-install a package from its recorded ref

**Usage:** `tamias update [OPTIONS] [NAME]`

###### **Arguments:**

* `[NAME]` — `@scope/name` or `name` (omit with `--all`)

###### **Options:**

* `--all` — Update every installed package
* `--allow-drivers` — Consent to register + run bundled `drivers/` on re-install. Equivalent: `TAMIAS_ALLOW_DRIVERS=1` — a global env bypass honored by every surface (CLI, serve, TUI)
* `--no-bins` — Don't deploy declared `bins[]` on re-install
* `--json` — JSON output
* `-h, --help` — Print help

## `tamias pin`

Retarget an installed package's recorded source ref

**Usage:** `tamias pin <NAME> <REFERENCE>`

###### **Arguments:**

* `<NAME>` — `@scope/name` or `name`
* `<REFERENCE>` — The new ref to record

###### **Options:**

* `-h, --help` — Print help

## `tamias self-upgrade`

Update the tamias package this binary belongs to. `self-upgrade` answers "am I a tamias-managed binary?" first: the running executable is canonicalized and checked against the store root — `~/.local/bin/tamias` resolves through the `bin-alias`/`bin` symlink chain straight into the versioned entry dir, so one canonical path covers direct store installs, alias links, and bin-dir links alike. On copy platforms (where `tamias bin` copies rather than symlinks) the bin-dir copy is matched against entries' `bin/` payloads by file name. A cargo or distro install fails cleanly — there is no package to update. A managed binary re-installs its recorded source ref — the same path `tamias update <name>` takes. A release install pinned to a `#sha256:` ref fetches exactly those bytes; a moving ref re-resolves. The install retargets the bin alias, so the *next* invocation runs the new version — this process keeps the old binary until restarted (a running exe can't replace itself).

**Usage:** `tamias self-upgrade [OPTIONS]`

###### **Options:**

* `--allow-drivers` — Consent to register + run bundled `drivers/` on re-install. Equivalent: `TAMIAS_ALLOW_DRIVERS=1` — a global env bypass honored by every surface (CLI, serve, TUI)
* `--no-bins` — Don't deploy declared `bins[]` on re-install
* `--check` — Report current vs what the recorded ref resolves to, without installing
* `--json` — JSON output
* `-h, --help` — Print help (see a summary with '-h')

## `tamias bin`

Manage a package's executable surfaces — the declared `bins[]` (generated `cli` shims + `tools/bin/<triple>/` payloads) and `bin/` payload files — on the bin dir (`$TAMIAS_BIN_DIR` → `XDG_BIN_HOME` → `~/.local/bin`). `install` deploys declared bins by default (`--no-bins` opts out); `bin deploy`/`bin destroy` are the explicit lifecycle. Deployed payload bins link through a stable per-package `bin-alias/` dir so upgrades don't break them; shims are tiny scripts that exec `tamias bin exec`, which re-resolves the kind driver at call time. Name collisions warn and last-deploy wins.

**Usage:** `tamias bin <COMMAND>`

###### **Commands:**

* `list` — List every installed package's declared `bins[]` + deploy state (which package owns each bin-dir name, collisions flagged)
* `deploy` — Deploy a package's executable surfaces into the bin dir — declared `bins[]` (cli shims + `tools/bin/` payloads) and `bin/` payload files. `install` does this by default; `--no-bins` opts out
* `destroy` — Remove a package's deployed bins — the pack stays installed. Removes only artifacts this package owns; `--force` also clears foreign files at the bin paths

###### **Options:**

* `-h, --help` — Print help (see a summary with '-h')

## `tamias bin list`

List every installed package's declared `bins[]` + deploy state (which package owns each bin-dir name, collisions flagged)

**Usage:** `tamias bin list [OPTIONS]`

###### **Options:**

* `--json` — JSON output
* `-h, --help` — Print help

## `tamias bin deploy`

Deploy a package's executable surfaces into the bin dir — declared `bins[]` (cli shims + `tools/bin/` payloads) and `bin/` payload files. `install` does this by default; `--no-bins` opts out

**Usage:** `tamias bin deploy [OPTIONS] <NAME>`

###### **Arguments:**

* `<NAME>` — `@scope/name` or `name` (a version label picks one install)

###### **Options:**

* `--force` — Overwrite existing files at bin paths (and other packages' deployed bins — those warn and lose anyway)
* `--json` — JSON output
* `-h, --help` — Print help

## `tamias bin destroy`

Remove a package's deployed bins — the pack stays installed. Removes only artifacts this package owns; `--force` also clears foreign files at the bin paths

**Usage:** `tamias bin destroy [OPTIONS] <NAME>`

###### **Arguments:**

* `<NAME>` — `@scope/name` or `name` (a version label picks one install)

###### **Options:**

* `--force` — Remove foreign files at the bin paths too
* `--json` — JSON output
* `-h, --help` — Print help

## `tamias cache`

Fetch-cache maintenance

**Usage:** `tamias cache <COMMAND>`

###### **Commands:**

* `clean` — Evict the fetch cache by age, or everything

###### **Options:**

* `-h, --help` — Print help

## `tamias cache clean`

Evict the fetch cache by age, or everything

**Usage:** `tamias cache clean [OPTIONS]`

###### **Options:**

* `--all` — Evict everything regardless of age
* `--days <DAYS>` — Max age in days (default 30)
* `--json` — JSON output
* `-h, --help` — Print help

## `tamias version`

Print the tamias version

**Usage:** `tamias version`

###### **Options:**

* `-h, --help` — Print help

## `tamias manifest`

Print the machine-readable capability manifest

**Usage:** `tamias manifest`

###### **Options:**

* `-h, --help` — Print help

## `tamias schema`

Print the `tamias.yaml` JSON Schema

**Usage:** `tamias schema`

###### **Options:**

* `-h, --help` — Print help

## `tamias gen-cli-docs`

Print the CLI reference (markdown) generated from the parser

**Usage:** `tamias gen-cli-docs`

###### **Options:**

* `-h, --help` — Print help

## `tamias completions`

Print a shell completion script

**Usage:** `tamias completions <SHELL>`

###### **Arguments:**

* `<SHELL>` — The shell to generate completions for [possible values: bash, elvish, fish, powershell, zsh]

###### **Options:**

* `-h, --help` — Print help

## `tamias doctor`

Diagnose the host metaservice (store, cache, registry, drivers)

**Usage:** `tamias doctor [OPTIONS]`

###### **Options:**

* `--json` — JSON output
* `-h, --help` — Print help

## `tamias config`

Show the effective config — YAML (`tamias.yml`) is the standard

**Usage:** `tamias config [OPTIONS]`

###### **Options:**

* `--file <FILE>` — Explicit config file (overrides discovery)
* `--json` — JSON output
* `-h, --help` — Print help

## `tamias serve`

Run the ndjson JSON-RPC service over stdio

**Usage:** `tamias serve`

###### **Options:**

* `-h, --help` — Print help

## `tamias publish`

Publish a package dir to a registry

**Usage:** `tamias publish [OPTIONS] <DIR>`

###### **Arguments:**

* `<DIR>` — Payload dir (holds `tamias.yaml`)

###### **Options:**

* `--registry <REGISTRY>` — Registry root or http(s) base URL (default `~/.local/share/tamias/registry`)
* `--sign` — Sign the package (`ssh-keygen -Y`, namespace `tamias-package`) — the signature rides the registry index to `cf:` installs
* `--key <KEY>` — Signing key path — overrides `$TAMIAS_SIGNING_KEY`, `signing.key_env`, `signing.key`, `~/.ssh/id_ed25519`
* `--keep-includes` — Ship the raw payload — don't flatten `include:` bases into the packed tarball (default resolves them read-only)
* `--keep <KEEP>` — Keep only the N most recent indexed versions of the package — overrides `retention.keep` config for this publish
* `--json` — JSON output
* `-h, --help` — Print help

## `tamias search`

Search a registry's index

**Usage:** `tamias search [OPTIONS] <QUERY>`

###### **Arguments:**

* `<QUERY>` — Name/scope substring

###### **Options:**

* `--registry <REGISTRY>` — Registry root or http(s) base URL
* `--json` — JSON output
* `-h, --help` — Print help

## `tamias bundle`

Author-side packing

**Usage:** `tamias bundle <COMMAND>`

###### **Commands:**

* `pack` — Pack a payload dir into a `.tgz`

###### **Options:**

* `-h, --help` — Print help

## `tamias bundle pack`

Pack a payload dir into a `.tgz`

**Usage:** `tamias bundle pack [OPTIONS] <DIR>`

###### **Arguments:**

* `<DIR>` — The payload dir (holds `tamias.yaml`)

###### **Options:**

* `--out <OUT>` — Output `.tgz` (default `<name>-<version>.tgz`)
* `--keep-includes` — Ship the raw payload — don't flatten `include:` bases into the packed tarball (default resolves them read-only)
* `-h, --help` — Print help

## `tamias registry`

Registry operations — named sources in the config file

**Usage:** `tamias registry [OPTIONS] <COMMAND>`

###### **Commands:**

* `serve` — Serve a local registry over HTTP
* `add` — Add a named registry to the config file
* `remove` — Remove a named registry from the config file
* `get` — Show one named registry
* `list` — List named registries

###### **Options:**

* `--file <FILE>` — Explicit config file for the management verbs (overrides `TAMIAS_CONFIG` + discovery)
* `-h, --help` — Print help

## `tamias registry serve`

Serve a local registry over HTTP

**Usage:** `tamias registry serve [OPTIONS]`

###### **Options:**

* `--addr <ADDR>` — Bind address [default: 127.0.0.1:8788]
* `--dir <DIR>` — Registry root (default `~/.local/share/tamias/registry`)
* `--max-upload <MAX_UPLOAD>` — Upload cap — bytes or a human size (`256MiB`, `1GiB`). Overrides the config's global `limits.max_upload`; the built-in 512 MiB default applies when neither is set
* `-h, --help` — Print help

## `tamias registry add`

Add a named registry to the config file

**Usage:** `tamias registry add [OPTIONS] --kind <KIND> --url <URL> <NAME>`

###### **Arguments:**

* `<NAME>` — Registry name — `publish`/`search --registry <name>` and the `publish.default`/`search.default` routing resolve it

###### **Options:**

* `--kind <KIND>` — Registry kind
* `- local: A local registry dir (`LocalRegistry`, optionally `registry serve`d)`
* `- http:` — Any http(s) blob-store endpoint
* `- cf:` — A Cloudflare-Worker-managed registry (R2 + bearer token)
* `--url <URL>` — Base URL (http/cf) or root dir (local)
* `--token-env <TOKEN_ENV>` — Env var NAME holding the bearer token (http/cf) — the value is read from the environment at connect time, never stored
* `--id <ID>` — Stable identifier (uuid or slug-safe string) — registry-scoped install refs (`<id>:<name>@<version>`) match it before the registry name
* `-h, --help` — Print help (see a summary with '-h')

## `tamias registry remove`

Remove a named registry from the config file

**Usage:** `tamias registry remove <NAME>`

###### **Arguments:**

* `<NAME>` — Registry name

###### **Options:**

* `-h, --help` — Print help

## `tamias registry get`

Show one named registry

**Usage:** `tamias registry get [OPTIONS] <NAME>`

###### **Arguments:**

* `<NAME>` — Registry name

###### **Options:**

* `--json` — JSON output
* `-h, --help` — Print help

## `tamias registry list`

List named registries

**Usage:** `tamias registry list [OPTIONS]`

###### **Options:**

* `--json` — JSON output
* `-h, --help` — Print help

## `tamias deploy-registry`

Deploy the registry Worker (Cloudflare)

**Usage:** `tamias deploy-registry [OPTIONS]`

###### **Options:**

* `--out <OUT>` — Output dir (default `./tamias-registry-worker`)
* `-h, --help` — Print help

## `tamias deploy`

Deploy an installed package's service/runtime (via its provider)

**Usage:** `tamias deploy [OPTIONS] <NAME>`

###### **Arguments:**

* `<NAME>` — `@scope/name` or `name`

###### **Options:**

* `--config <CONFIG>` — Node/config file the provider reads operator params from
* `--set <KEY=VALUE>` — Persist an operator config override — `key=value`, dotted keys build nested maps, values parse as YAML. Repeatable; stored under `<config>/tamias/deploy.d/<kind>/<name>/cli.yaml` and handed to the driver on every deploy/install
* `--unset <KEY>` — Remove a persisted override by dotted key. Repeatable
* `--clear-overrides` — Drop all persisted overrides for this package
* `--show-overrides` — Print the persisted overrides (post --set/--unset/--clear) and exit without deploying
* `-h, --help` — Print help

## `tamias uninstall`

Uninstall — provider teardown + drop the store entry

**Usage:** `tamias uninstall <NAME>`

###### **Arguments:**

* `<NAME>` — `@scope/name` or `name`

###### **Options:**

* `-h, --help` — Print help

## `tamias service`

Manage a per-user service unit (systemd)

**Usage:** `tamias service <COMMAND>`

###### **Commands:**

* `install` — Write a per-user service unit (does not enable it)
* `enable` — Enable + start the unit
* `disable` — Disable + stop the unit
* `status` — Unit status
* `logs` — Follow the unit's logs
* `uninstall` — Disable + remove the unit file

###### **Options:**

* `-h, --help` — Print help

## `tamias service install`

Write a per-user service unit (does not enable it)

**Usage:** `tamias service install [OPTIONS] --unit <UNIT> --exec <EXEC>`

###### **Options:**

* `--unit <UNIT>` — Unit base name
* `--exec <EXEC>` — `ExecStart`
* `--description <DESCRIPTION>` — Unit description [default: "tamias service"]
* `--env <ENVS>` — `K=V` environment — repeatable
* `--env-file <ENV_FILE>` — `EnvironmentFile=` path (`~` expands; rendered optional `-`)
* `--restart <RESTART>` — `Restart=` policy (default `on-failure`)
* `--workdir <WORKDIR>` — Working directory (`~` expands to `$HOME`)
* `-h, --help` — Print help

## `tamias service enable`

Enable + start the unit

**Usage:** `tamias service enable <UNIT>`

###### **Arguments:**

* `<UNIT>`

###### **Options:**

* `-h, --help` — Print help

## `tamias service disable`

Disable + stop the unit

**Usage:** `tamias service disable <UNIT>`

###### **Arguments:**

* `<UNIT>`

###### **Options:**

* `-h, --help` — Print help

## `tamias service status`

Unit status

**Usage:** `tamias service status <UNIT>`

###### **Arguments:**

* `<UNIT>`

###### **Options:**

* `-h, --help` — Print help

## `tamias service logs`

Follow the unit's logs

**Usage:** `tamias service logs [OPTIONS] <UNIT>`

###### **Arguments:**

* `<UNIT>`

###### **Options:**

* `--lines <LINES>` — Journal lines to show [default: 200]
* `-f, --follow` — Follow the journal (`journalctl -f`)
* `-h, --help` — Print help

## `tamias service uninstall`

Disable + remove the unit file

**Usage:** `tamias service uninstall <UNIT>`

###### **Arguments:**

* `<UNIT>`

###### **Options:**

* `-h, --help` — Print help

## `tamias drivers`

List discovered content drivers (`tamias-driver-<type>` on PATH + `<tamias-home>/drivers/`)

**Usage:** `tamias drivers [OPTIONS]`

###### **Options:**

* `--json` — JSON output
* `-h, --help` — Print help

## `tamias driver`

Provider-verb dispatch — run a driver verb beyond the lifecycle three (`install`/`deploy`/`uninstall`)

**Usage:** `tamias driver <COMMAND>`

###### **Commands:**

* `exec` — Dispatch a provider verb to an installed package's kind driver — `tamias driver exec <pkg> <verb> [args…]`. The verb and any args pass through verbatim after the standard `--dir`/`--name` flags; the driver's stdout/stderr stream back. `tamias drivers` lists each driver's declared verbs; `tamias run` stays the local service-exec surface — provider-specific ops (hook registration, health probes, unit status) live here
* `config-schema` — Fetch the `--config` file's JSON Schema from the package's kind driver — the reserved `config-schema` verb, printed to stdout. Drivers without one exit non-zero; tamias surfaces their stderr ("unsupported" is expected for drivers that don't declare it). Validate a config driver-side: `tamias driver exec <pkg> config validate --config <file>` — tamias ships no schema validator

###### **Options:**

* `-h, --help` — Print help

## `tamias driver config-schema`

Fetch the `--config` file's JSON Schema from the package's kind driver — the reserved `config-schema` verb, printed to stdout. Drivers without one exit non-zero; tamias surfaces their stderr ("unsupported" is expected for drivers that don't declare it). Validate a config driver-side: `tamias driver exec <pkg> config validate --config <file>` — tamias ships no schema validator

**Usage:** `tamias driver config-schema [OPTIONS] <NAME>`

###### **Arguments:**

* `<NAME>` — `@scope/name` or `name` — the installed package

###### **Options:**

* `--config <CONFIG>` — Node/config file passed through as the driver's `--config`
* `-h, --help` — Print help

## `tamias provider`

Provider packages — the declarative `provider:` runtime spec

**Usage:** `tamias provider <COMMAND>`

###### **Commands:**

* `list` — List installed packages' `provider:` blocks — services (with unit state), ingress URLs, verbs, health probe

###### **Options:**

* `-h, --help` — Print help

## `tamias provider list`

List installed packages' `provider:` blocks — services (with unit state), ingress URLs, verbs, health probe

**Usage:** `tamias provider list [OPTIONS]`

###### **Options:**

* `--all` — Also list packages without a `provider:` block
* `--json` — JSON output
* `-h, --help` — Print help

## `tamias run`

Run an installed provider service's exec in the foreground — the unit's argv + env, without systemd. `tamias run <pkg> [service] [-- extra-args]`

**Usage:** `tamias run <NAME> [SERVICE] [-- <ARGS>...]`

###### **Arguments:**

* `<NAME>` — `@scope/name` or `name` — the package must declare `provider:`
* `[SERVICE]` — Service name — required when the provider declares several
* `[ARGS]...` — Extra argv appended to the service exec (after `--`)

###### **Options:**

* `-h, --help` — Print help

## `tamias logs`

Follow a package's provider units' journals — `tamias logs <pkg|unit>`. A package name resolves to all its provider units; anything else is treated as a unit base name

**Usage:** `tamias logs [OPTIONS] <NAME>`

###### **Arguments:**

* `<NAME>` — Package name or unit base name

###### **Options:**

* `--lines <LINES>` — Journal lines to show [default: 200]
* `-f, --follow` — Follow the journal (`journalctl -f`)
* `-h, --help` — Print help

## `tamias ui`

Interactive package-manager TUI (ratatui) — installed entries, registry search, provider lifecycle, manifest deps, authoring

**Usage:** `tamias ui`

###### **Options:**

* `-h, --help` — Print help
