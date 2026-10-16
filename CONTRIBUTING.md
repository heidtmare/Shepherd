# Contributing

Before opening a pull request:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --workspace -- --ignored   # Docker; recognition tests also need scripts/fetch-deps.sh
```

Event consumers are declared in `shepherd-stream/config/topology.toml`. Schema changes go in a new migration under the owning crate's `migrations/`; existing migrations are not edited.

Contributions are dual-licensed under MIT or Apache 2.0 unless stated otherwise.
