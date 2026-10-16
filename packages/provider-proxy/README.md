# Proxy provider package

Optional native provider package for rpi. The CLI does not link this crate.

From the repository root:

```sh
rpi package link proxy --path packages/provider-proxy --debug
```

See [Provider packages](../../docs/architecture/provider-packages.md) for credentials,
models.json overrides, streaming, cancellation and reload behavior.
