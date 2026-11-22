# Contributing

Describe a concrete observed problem and its expected behavior. Include the source
revision and a minimal, sanitized reproduction. Do not attach real credentials,
account data, private discussions or proprietary datasets.

Before opening a pull request, run:

```sh
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Add focused regression coverage for behavior changes. Preserve raw archives,
first-arrival timing, explicit missing-data states and the read-only CLI boundary.
Do not interpret an HTTP success, quote submission or timeout as proof of a fill.
Live-feed or timing claims need reproducible evidence and clear limitations.

Contributions are made under this repository's MIT license.
