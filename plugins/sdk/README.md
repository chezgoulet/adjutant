# adjutant-sdk

The contract between the [Adjutant](https://github.com/chezgoulet/adjutant) core
and every plugin — first-party and third-party alike.

A plugin implements `AdjutantPlugin`, declares its routes, permissions, and
migrations in code, and calls the core's services through host-mediated traits
(no `sqlx` or `tokio` is linked into a plugin). See the
[plugin development guide](https://github.com/chezgoulet/adjutant/blob/testing/docs/plugin-development.md).

```toml
[dependencies]
adjutant-sdk = "0.2"
```

```rust
use std::sync::OnceLock;
use adjutant_sdk::prelude::*;

pub struct MyPlugin { ctx: OnceLock<PluginContext> }

#[async_trait]
impl AdjutantPlugin for MyPlugin {
    fn id(&self) -> &str { "my_plugin" }
    fn name(&self) -> &str { "My Plugin" }
    fn version(&self) -> &str { env!("CARGO_PKG_VERSION") }

    async fn init(&mut self, ctx: PluginContext) -> Result<(), SdkError> {
        let _ = self.ctx.set(ctx);
        Ok(())
    }

    fn routes(&self) -> Vec<RouteDefinition> { /* … */ }
}

export_plugin!(MyPlugin);
```

Permissions and migrations are declared **once** with `permissions!` and
`migrations!`: the ids, the `permissions_granted()` list, the migration identity
and the `.sql` files are each written in one place, and the macros generate the
rest — including a test that every route gate names a permission the crate
declared, and a compile error on a duplicate migration version or a missing
`.sql` file.

```rust
pub mod perms {
    adjutant_sdk::permissions! {
        READ = "my_plugin:read" => "Read my plugin's data";
    }
}
pub mod migrations {
    adjutant_sdk::migrations! {
        1 => "my_plugin_schema" => "../migrations/001_my_plugin_schema.sql";
    }
}

fn permissions_granted(&self) -> Vec<Permission> { perms::granted() }
fn migrations(&self) -> Vec<Migration> { migrations::all() }
```

The core and SDK share a version; a plugin built against a different SDK ABI is
refused at load with a clear error. See
[`CHANGELOG.md`](https://github.com/chezgoulet/adjutant/blob/testing/CHANGELOG.md)
and the [compatibility policy](https://github.com/chezgoulet/adjutant/blob/testing/docs/sdk-compatibility.md)
(the macros are additive — the ABI stays 4).

## License

MIT.
