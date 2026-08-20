# oc-clean

OpenCode Database Cleaner — a Rust CLI that manually prunes old OpenCode data and reclaims disk space.

## Why

OpenCode stores sessions, messages, and context in a SQLite database at `~/.local/share/opencode/opencode.db`. Nothing expires old sessions and nothing runs `VACUUM`, so the file only grows — real-world databases reach tens of gigabytes.

`oc-clean` lets you delete data older than a chosen age and reclaim the freed pages.

## Development

Requires the Rust nightly toolchain.

```sh
cargo build
cargo nextest run
cargo llvm-cov --all-features nextest   # coverage (install cargo-llvm-cov first)
```

Contributor and agent conventions live in [AGENTS.md](AGENTS.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in oc-clean by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
