# adjutant-server

The [Adjutant](https://github.com/chezgoulet/adjutant) core server: plugin
registry and lifecycle, a WASM sandbox, Axum HTTP dispatch, an event bus, a
tamper-evident audit log, and per-plugin PostgreSQL schema isolation.

Installing this crate provides the `adjutant` binary (server + CLI):

```bash
cargo install adjutant-server

adjutant --help
adjutant new-plugin gear_locker          # scaffold a plugin
adjutant validate-plugin target/debug/libadjutant_gear_locker.so
adjutant serve                            # run (needs PostgreSQL)
```

See the [deployment guide](https://github.com/chezgoulet/adjutant/blob/testing/docs/deployment.md)
and the [architecture overview](https://github.com/chezgoulet/adjutant/blob/testing/docs/architecture.md).

> This crate depends on `adjutant-sdk`; publish the SDK first (see
> [releasing](https://github.com/chezgoulet/adjutant/blob/testing/docs/releasing.md)).

## License

MIT.
