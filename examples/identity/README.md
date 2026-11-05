# Identity processor example

A minimal Abner processor that returns each input frame unchanged and reports
progress. Use its `Factory` and `Session` implementations as a starting point
for a new plugin. It is outside Abner's default and composed plugin discovery.

From Abner's root, compile it with:

```sh
cargo check --manifest-path examples/identity/Cargo.toml
```

To try it in a composed build, link `examples/identity` directly under
`plugins/` and run `cargo build-with-plugins`.
