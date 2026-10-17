# rpi-provider-runtime

Shared synchronous C ABI bridge for optional provider packages. Exports a Rust
`Provider` factory through `export_provider!`, declares its model catalog, forwards
streaming events and honors host cancellation. Owns a per-request runtime so no
background worker survives plugin unloading. This crate is not linked by the CLI.
