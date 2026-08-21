# AGENTS.md

`oc-clean` — OpenCode Database Cleaner. A Rust CLI that manually prunes OpenCode's SQLite database (sessions/messages/context older than N days) and reclaims disk space. OpenCode never vacuums or expires anything, so real-world databases reach tens of gigabytes.

## Rust skills

This project ships a `rust-skills` package, and it is the authority on how Rust is written here.

**Before writing or reviewing any Rust, load the matching skill and follow it.** This is mandatory, and it applies to sub-agents exactly as it applies to you. Do not fall back on general Rust knowledge when a skill covers the topic, and do not restate or override skill guidance in this file.

`opencode.jsonc` also loads `docs/instructions/rust-skills.md`, which maps error codes and topics to the individual skills.

## Toolchain

`rust-toolchain.toml` pins nightly with `rustfmt`, `clippy`, and `rust-src`, so plain `cargo` already selects the right toolchain.

`Cargo.toml` must keep `edition = "2024"` and `rust-version = "1.85"`, plus the lint block mandated by rust-skills:

```toml
[lints.rust]
unsafe_code = "warn"

[lints.clippy]
all = "warn"
pedantic = "warn"
```

`cargo-nextest`, `cargo-llvm-cov`, `cargo-audit`, and `task` (go-task) are all installed.

## Commands

`Taskfile.yml` is the task runner and the shortest path to the right invocation. `task` alone lists everything.

```sh
task ci            # fmt:check -> lint -> test, the full local gate
task test          # cargo nextest run --no-tests=pass
task test:doc      # doctests; nextest does not run them and CI checks them separately
task coverage      # line coverage, floor is 80 percent
task lint          # clippy --all-targets --all-features -- -D warnings
task build:release # optimized binary
```

Run a single test with `cargo nextest run -E 'test(name_substring)'`, or `task test -- -E '...'`.

The `--no-tests=pass` flag matters: several test binaries are feature-gated and compile to zero tests without `--all-features`, and nextest treats an empty binary as failure by default.

## Dependencies

Any crate is allowed, but favor stable, widely-adopted ones. Recently released crates are acceptable when they are clearly maintained; pre-1.0 churn-heavy or abandoned crates are not.

The binary is **fully synchronous** and has no async runtime. `tokio`, `anyhow`, `dirs`, and `ratatui` are excluded by design; propose an alternative before reaching for any of them.

Established choices: `clap` (derive + `env` features) for the CLI, `rusqlite` (`bundled`, `hooks`) for SQLite, `thiserror` for the error enum, `tracing` + `tracing-subscriber` for diagnostics, `indicatif` for progress, `ctrlc` for interrupt handling, `fs2` for free-space checks, `rustix` for openat-based traversal on Unix. JSON is emitted through `serde_json` values built by hand; there is no `serde` derive dependency.

Holder detection is per-platform and its dependencies are target-gated: `procfs` on Linux, `libproc` on macOS, `windows` (Restart Manager) on Windows.

## Architecture

`src/main.rs` is a thin dispatcher: parse `Cli`, initialize logging, route to one of four command entrypoints, then map `Error::exit_code()` onto the process exit status. Everything testable lives in `src/lib.rs` modules.

Four subcommands, four entrypoints:

| Command | Entrypoint | Mutates |
|---|---|---|
| `analyze` | `src/report/command.rs` | no |
| `doctor` | `src/doctor/command.rs` | no |
| `clean` | `src/clean/command.rs` | with `--apply` |
| `vacuum` | `src/reclaim/command.rs` | with `--apply` |

Supporting modules split by responsibility: `select/` chooses sessions (predicates, retention, subtree expansion, orphans), `delete/` performs bounded deletion, `assets/` handles external storage and snapshot directories, `reclaim/` implements `VACUUM INTO` plus incremental vacuum and headroom math, `safety/` covers holder detection and confirmation, `db/` owns connections and schema tiering, `paths/` resolves the database and its sibling directories.

`src/error.rs` holds the single `Error` enum. Its variants are the stable process contract; `exit_code()` is the only mapping and a test asserts the enum matches the documented table.

Two safety invariants that are easy to violate:

- **Dry run and apply must select identically.** The only difference is whether mutation happens.
- **Never call `fs::canonicalize`.** Path identity is anchored through opened handles and openat-style traversal instead, which is what prevents symlink escape during backup and swap. There are currently zero occurrences in `src/`; keep it that way.

## Schema fixture

`src/db/opencode_schema.sql` is extracted from the OpenCode binary, never hand-written. Its header records the exact source version, sha256, and extraction commands. The file carries `-- @shape upgraded`, `-- @shape fresh`, and `-- @end` markers because OpenCode's schema differs between a fresh install and an upgraded database; both shapes must keep working.

`src/db/mod.rs` and `tests/support/fixture.rs` both `include_str!` this file, so a schema edit changes production behavior and every fixture at once. Regenerate it from a real OpenCode binary rather than editing DDL by hand.

## Testing

**TDD is the default.** Unless the user gives a different testing strategy for a task, write the failing test first, then the implementation. This applies to sub-agents too.

**Line coverage target: 80%.** CI enforces it with `--fail-under-lines 80`.

Unit tests live in `mod tests` beside the code. `tests/` holds integration tests, and `tests/e2e/` drives the compiled binary through `env!("CARGO_BIN_EXE_oc-clean")` — end-to-end coverage must go through the real process so exit codes and stdout/stderr separation are exercised, never through library calls.

Never point tests at a real OpenCode database. Build throwaway SQLite fixtures in `tempfile` directories; destructive behavior (deletes, `VACUUM`) must be proven against fixtures only.

Multi-gigabyte benchmark tests are gated behind the `bench-large` feature and `#[ignore]`, so a normal run skips them. They also enable the `gen-baseline` helper binary. Run one explicitly:

```sh
cargo test --release --features bench-large --test perf performance_budgets_hold_on_the_committed_large_fixture_scale -- --ignored --nocapture --test-threads=1
```

Coverage is a floor, not a goal — do not pad it with tests that assert nothing.

## Documentation is tested

`tests/readme.rs` compiles `README.md` in and asserts against the source. Editing the README carelessly breaks the build:

- The **Options** table must match the clap interface exactly, in both directions — every long flag, its `<VALUE_NAME>`, and its env binding or `CLI only`.
- The **Exit Codes** table must cover every variant of `Error` in `src/error.rs`, with the same codes.
- README prose must be **ASCII** and must not soft-wrap; two consecutive prose lines fail the test.
- The **Roadmap** section must keep naming AFT and Magic Context.

Adding or renaming a CLI flag or an error variant therefore requires a README edit in the same change.

`docs/json-report.md` documents the `--json` contract. `schema_version` is the compatibility boundary: additive fields are fine within a version, while removals, renames, and semantic changes require an increment.

## Markdown

Do not soft-wrap Markdown. Line-width limits configured for code do not apply to `.md` files — write each paragraph as a single long line and let the editor wrap it. Line breaks belong only where the document structure needs them: between paragraphs, list items, and code blocks.

## Commits

Agents and sub-agents commit their own work. No exceptions to the format:

- Conventional Commits with optional scope: `type(scope): subject`
- **Single line only.** No body, no footer, no trailer, no co-author line.
- Allowed types: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `chore`, `build`, `ci`, `style`, `revert`.
- Anything else — plain prose subjects, multi-line messages, emoji — is rejected.

```
feat(cli): add --older-than flag
fix(db): close connection before vacuum
test(prune): cover cascade delete of orphan parts
```

Never mention agents, models, tooling, phases, or task IDs in a commit message.

release-please consumes these commits: `feat` and `fix` drive the version bump and the visible changelog sections, while `test`, `build`, `ci`, `chore`, and `style` are hidden. A sloppy type produces a wrong release.

## CI

`.github/workflows/ci.yml` runs three jobs. The test job is a native matrix across Ubuntu, macOS, and Windows because holder detection has a separate implementation per platform and only a native host exercises it. It runs fmt, clippy, build, nextest, and doctests. A separate job enforces the coverage floor, and a third runs `cargo audit --deny warnings`, so a new advisory in the dependency tree fails CI even when no code changed.

`task ci` reproduces the fmt/lint/test portion locally.

## Repo boundaries

Never commit `.omo/` or `.sisyphus/`.

Do not connect to, read, or modify the user's live OpenCode database (`~/.local/share/opencode/opencode.db`) while developing. Destructive operations against real user data happen only when the user explicitly runs the built CLI.

`.agents/skills/` and `skills-lock.json` are vendored skill sources — edit them only when the task is explicitly about skill maintenance.

`benchmarks.json` is committed measured data used as a regression reference by the large-fixture perf test. Update it from an actual run, never by hand.
