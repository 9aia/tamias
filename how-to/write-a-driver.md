# How to write a content driver

A **driver** teaches tamias how a content *type* installs, deploys, and
uninstalls — with **no compile-time coupling**. tamias discovers
`tamias-driver-<type>` on `PATH` or `<tamias-home>/drivers/`,
and speaks JSON over stdio.

## The contract

One request line in, one response line out:

| Request | Meaning |
| --- | --- |
| `{"op":"capabilities"}` | JSON with the `types` you handle |
| `{"op":"install","dir":…,"name":…,"config":…}` | materialize |
| `{"op":"deploy",…}` | activate (unit, hooks) |
| `{"op":"uninstall",…}` | tear down |

`config` is an optional path to a node/config file carrying operator
params. A driver that exits non-zero fails the tamias verb.

## Capabilities

tamias maps a package's `type:` to a driver. Either name the binary
`tamias-driver-<type>`, or answer `capabilities` so a differently-named
binary can claim types:

```sh
#!/bin/sh
if [ "$1" = capabilities ]; then
  echo '{"types":["myco-config"]}'
  exit 0
fi
# …handle install/deploy/uninstall…
```

## Ship it with the payload

Drop the binary under `drivers/` in your payload — installing the package
registers it into `<tamias-home>/drivers/`, and tamias finds it:

```
mypkg/
  tamias.yaml         # type: myco-config
  drivers/
    tamias-driver-myco
```

## Check

```sh
tamias doctor --json    # lists discovered drivers
```

## See also

- [Content types](/explanation/content-types)
- [Provision and service](/how-to/provision-a-service)
