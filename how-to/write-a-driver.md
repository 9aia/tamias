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
| `capabilities` | print `{"types":[…]}` on stdout — the types this binary handles |

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

Drop the binary under `drivers/` in your payload — but know that this
makes the package **self-modifying**: a bundled driver is arbitrary code
that runs on the installer's machine. tamias therefore gates it:

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

## Check

```sh
tamias doctor --json    # lists discovered drivers
```

## See also

- [Content types](/explanation/content-types)
- [Provision and service](/how-to/provision-a-service)
