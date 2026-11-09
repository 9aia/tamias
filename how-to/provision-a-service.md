# How to provision and service-manage a package

`install` puts a payload in the store. **Provisioning** (materializing a
runnable artifact), **deploy** (activation), and the **service unit** are
the driver's job — tamias shells the driver and never learns its schema.

## Deploy an installed package

```sh
tamias install git:9aia/peregrin//libraries/ci-gate#main
tamias deploy ci-gate                       # runs the driver's deploy step
tamias deploy ci-gate --config ./node.yaml   # operator params ride a config file
```

A package that ships its own `drivers/` needs `--allow-drivers` at
install time (or `TAMIAS_ALLOW_DRIVERS=1`) — the consent gate is
described in [Security model](/explanation/security).

If the package's type has no driver, `deploy` reports
`nothing to deploy (type …)` and succeeds. `uninstall` runs the driver's
teardown first; with no driver it warns that provider-side resources may
remain, then drops the store entry.

## Manage the service unit

The per-user **systemd** mechanism:

```sh
tamias service install --unit ci-gate \
  --exec "/usr/local/bin/ci-gate run" \
  --description "ci-gate" \
  --env HOME=/home/me \
  --workdir /srv/ci-gate
tamias service enable  ci-gate
tamias service status  ci-gate
tamias service logs    ci-gate
tamias service disable ci-gate
tamias service uninstall ci-gate
```

`service install` writes the unit (it does **not** enable it). `uninstall`
disables + removes the unit file.

## Tear down

```sh
tamias uninstall ci-gate    # driver teardown + drop the store entry
```

## See also

- [Write a driver](/how-to/write-a-driver)
- [Content types](/explanation/content-types)
