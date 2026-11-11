---
url: /tamias/how-to/write-a-driver.md
---
# How to write a content driver

A **driver** teaches tamias how a content *type* installs, deploys, and
uninstalls — with **no compile-time coupling**. tamias discovers
`tamias-driver-<type>` on `PATH` or `<tamias-home>/drivers/` and runs
it as a subprocess.

## The contract

```text
tamias-driver-<type> <verb> --dir <dir> --name <name> [--config <file>]
```

| verb | Meaning |
| --- | --- |
| `install` | materialize the payload at `--dir` |
| `deploy` | activate (unit, hooks) |
| `uninstall` | tear down |
| `capabilities` | print `{"types":[…],"verbs":[…]}` on stdout — the types + verbs this binary handles |

## Ops verbs beyond the lifecycle three

`install`/`deploy`/`uninstall` are the only verbs tamias dispatches on
its own. A driver may declare **provider ops verbs** — `health`,
`status`, `hooks register`, anything the product needs — by listing them
in `capabilities.verbs`; the operator reaches them through the generic
passthrough:

```sh
tamias driver exec <pkg> <verb> [args…]
# → tamias-driver-<type> <verb> --dir <dir> --name <name> [--config <f>] [args…]
```

Everything after `<verb>` lands verbatim as trailing argv, so verbs can
take their own flags (`--dry-run`, `--api-base`, …). The driver's
stdout/stderr stream back — status/probe verbs should print JSON or
report lines. `tamias drivers --json` shows each driver's declared
verbs; `tamias driver exec` warns (but still dispatches) when the verb
isn't declared — the driver stays the authority. Provider verbs that
answer with data are the right place for forge-hook registration,
health probes, and unit status; `tamias run` remains local service exec
only.

`--config` is an optional path to a node/config file carrying operator
params (`tamias deploy --config`). Exit zero on success; a non-zero
exit fails the tamias verb, with your stderr surfaced. Dispatch has a
120s timeout (`capabilities` probes get 10s), so a hung driver fails the
verb instead of wedging it.

## Capabilities

tamias maps a package's `type:` to a driver. Naming the binary
`tamias-driver-<type>` is enough — tamias tries that name first. A
differently-named binary can still claim types by answering
`capabilities`:

```sh
#!/bin/sh
if [ "$1" = capabilities ]; then
  echo '{"types":["myco-config"]}'
  exit 0
fi
# …handle install/deploy/uninstall…
```

## Ship it with the payload

Drop the driver under `drivers/` in your payload — two forms:

* `drivers/<name>` — a portable file (an interpreted script like
  `#!/usr/bin/env python3` ships as source);
* `drivers/bin/<target-triple>/<name>` — a compiled driver, one dir
  per rustc target triple (`x86_64-unknown-linux-gnu`, …). Only the
  host triple's file installs; a package that ships platform dirs but
  none for this host **fails the install** rather than silently
  dropping the driver.

Either way this makes the package **self-modifying**: a bundled driver
is arbitrary code that runs on the installer's machine. tamias
therefore gates it:

```sh
tamias install --allow-drivers <ref>
# or export TAMIAS_ALLOW_DRIVERS=1 once
```

Without consent the install **fails before anything is registered or
executed** — no partial store entry, no copied driver. Only regular
files directly under `drivers/` are registered, into
`<tamias-home>/drivers/`, where tamias discovers them.

```
mypkg/
  tamias.yaml         # type: myco-config
  drivers/
    tamias-driver-myco
```

## What if there's no driver?

A package whose `type:` has no discoverable driver **fails to install**
(any type other than `files`). tamias never silently "installs" content
it can't place — so either ship the driver, publish
`tamias-driver-<type>` on the host's `PATH`, or use `type: files`.

`tamias deploy`/`uninstall` behave the same way: they dispatch to the
driver for the package's type and warn when none exists.

## Trust boundary

Your driver binary runs with **full user privileges** over an
**untrusted `--dir` payload** — the payload came from a package ref you
may not control. Treat everything under `--dir` as hostile:

* **Never execute payload files yourself.** If the payload ships a
  script or binary, running it is the payload's privilege escalation,
  not yours.
* **Validate every path** you read or write — reject `..` traversal,
  absolute paths, and symlink escapes before touching the filesystem.
* **Don't trust manifest-declared paths or names** — a manifest is
  package-controlled data, not an instruction from the operator.

The consent gates (`--allow-drivers` / `TAMIAS_ALLOW_DRIVERS=1` /
per-package consent memory) decide whether tamias *registers* a bundled
driver — they are **not a sandbox**. Your driver is the last line of
defense.

## Check

```sh
tamias drivers          # every discovered driver + its capabilities
tamias drivers --json   # same, machine-readable
tamias doctor --json    # lists discovered drivers among the health checks
```

## Beyond the lifecycle: pavo surfaces

The lifecycle three plus `capabilities` are the *tamias* contract.
Drivers built on `pavo-driver-kit` additionally answer the **pavo**
surface — `manifest`, `serve` (ndjson views/queries), `ops` (an ops
manifest of bounded verbs), and `run`/`cli` passthroughs — which is how
product CLIs become generatable surfaces: `tamias-driver-tamias`
itself exposes the whole `tamias` engine this way (`cli.<verb>` ops
derived from the clap tree; unbounded verbs like `serve`/`ui` are
`surfaces`, never ops — see
[Scripting tamias](/reference/scripting#the-engine-as-an-ops-manifest)).
A `capabilities` answer of `{"capabilities": ["pavo"], …}` marks a
driver as pavo-capable; `types: []` keeps it from claiming content
types it doesn't handle.

## See also

* [Content types](/explanation/content-types)
* [Provision and service](/how-to/provision-a-service)
