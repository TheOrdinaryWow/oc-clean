# Contributing to oc-clean

`oc-clean` deletes data from a database people care about, so the bar for changes is correctness first and convenience second. This document covers the workflow, the quality gates, and the few project rules that are enforced by tests rather than by review.

The repository lives at <https://github.com/TheOrdinaryWow/oc-clean>. Report bugs and request features through its issue tracker.

## Getting Started

`rust-toolchain.toml` pins the nightly toolchain along with `rustfmt`, `clippy`, and `rust-src`, so rustup installs and selects it automatically for any cargo command run inside the checkout. No manual `rustup override` is needed.

```sh
git clone https://github.com/TheOrdinaryWow/oc-clean
cd oc-clean
cargo build
```

SQLite is compiled from source through `rusqlite`'s `bundled` feature, so no system SQLite development package is required. The first build is slower because of that C compilation step.

Four optional tools cover the full local gate. Install whichever you are missing:

```sh
cargo install cargo-nextest --locked
cargo install cargo-llvm-cov --locked
cargo install cargo-audit --locked
# task (go-task) from https://taskfile.dev
```

## Everyday Commands

`Taskfile.yml` is the task runner. `task` with no arguments lists every available target.

```sh
task ci            # fmt:check, then lint, then test; the full local gate
task test          # cargo nextest run --no-tests=pass
task test:doc      # doctests, which nextest does not run
task coverage      # line coverage against the 80 percent floor
task lint          # clippy --all-targets --all-features -- -D warnings
task fmt           # format the workspace in place
task audit         # dependency advisory scan
task build:release # optimized binary
```

Run a single test by substring:

```sh
cargo nextest run -E 'test(exit_code)'
task test -- -E 'test(exit_code)'
```

The `--no-tests=pass` flag is not decorative. Several test binaries are feature-gated and compile to zero tests without `--all-features`, and nextest treats an empty test binary as a failure by default.

## Project Layout

`src/main.rs` is a thin dispatcher: it parses the CLI, initializes logging, routes to one of four command entrypoints, and maps `Error::exit_code()` onto the process exit status. Everything testable lives behind `src/lib.rs`.

| Command | Entrypoint | Mutates the database |
|---|---|---|
| `analyze` | `src/report/command.rs` | no |
| `doctor` | `src/doctor/command.rs` | no |
| `clean` | `src/clean/command.rs` | only with `--apply` |
| `vacuum` | `src/reclaim/command.rs` | only with `--apply` |

The supporting modules divide by responsibility: `select/` chooses sessions through predicates, retention, subtree expansion, and orphan detection; `delete/` performs bounded deletion; `assets/` handles external storage and snapshot directories; `reclaim/` implements `VACUUM INTO`, incremental vacuum, and headroom arithmetic; `safety/` covers holder detection and confirmation; `db/` owns connections and schema tiering; `paths/` resolves the database and its sibling directories.

## Safety Invariants

These are the rules a change is most likely to break by accident.

- **Dry run and apply must select identically.** The only permitted difference between a preview and an applied run is whether mutation happens. End-to-end tests assert this.
- **Never call `fs::canonicalize`.** Path identity is anchored through opened handles and openat-style traversal, which is what prevents symlink escape during backup and swap. There are currently zero occurrences in `src/`.
- **Never point development or tests at a live OpenCode database.** Destructive behavior is proven against disposable fixtures only. The default database at `~/.local/share/opencode/opencode.db` is real user data.
- **Exit codes are a stable process contract.** The variants of `Error` in `src/error.rs` and their `exit_code()` mapping are public interface; changing one is a breaking change and requires the matching README edit.

## The Schema Fixture

`src/db/opencode_schema.sql` is extracted from a real OpenCode binary and must never be hand-written. Its header records the source version, its sha256, and the exact extraction commands used. The file carries `-- @shape upgraded`, `-- @shape fresh`, and `-- @end` markers because OpenCode's schema differs between a fresh installation and an upgraded database, and both shapes must keep working.

Both `src/db/mod.rs` and `tests/support/fixture.rs` pull the file in through `include_str!`, so one edit changes production behavior and every test fixture simultaneously. Regenerate it from a newer OpenCode binary rather than patching the DDL by hand.

## Testing

Test-driven development is the default: write the failing test first, then the implementation.

Unit tests live in a `mod tests` next to the code they cover. Integration tests live in `tests/`. Everything under `tests/e2e/` drives the compiled binary through `env!("CARGO_BIN_EXE_oc-clean")` rather than calling library functions, because exit codes and the stdout/stderr split are only observable through a real process.

Fixtures are throwaway SQLite databases created under `tempfile` directories. Destructive behavior must be exercised against those and nothing else.

Line coverage has a floor of 80 percent, enforced in CI with `--fail-under-lines 80`. Treat it as a floor rather than a target, and do not pad it with tests that assert nothing.

Multi-gigabyte benchmarks are gated behind the `bench-large` feature and `#[ignore]`, so an ordinary run skips them. The same feature enables the `gen-baseline` helper binary. Run the large-fixture regression explicitly:

```sh
cargo test --release --features bench-large --test perf performance_budgets_hold_on_the_committed_large_fixture_scale -- --ignored --nocapture --test-threads=1
```

`benchmarks.json` holds committed measurements used as the regression reference for that test. Update it from an actual run, never by editing numbers.

## Documentation Is Tested

`tests/readme.rs` includes `README.md` at compile time and asserts it against the source, so careless documentation edits fail the build:

- The **Options** table must match the clap interface exactly in both directions, covering every long flag, its `<VALUE_NAME>`, and either its environment binding or the literal `CLI only`.
- The **Exit Codes** table must cover every variant of `Error` in `src/error.rs` with matching codes.
- README prose must be ASCII and must not soft-wrap. Two consecutive prose lines fail the test.
- The **Roadmap** section must keep naming AFT and Magic Context.

Adding or renaming a CLI flag or an error variant therefore requires a README change in the same commit.

`docs/json-report.md` documents the `--json` contract. Its `schema_version` is the compatibility boundary: additive fields are acceptable within a version, while removals, renames, and semantic changes require an increment.

Markdown in this repository is not soft-wrapped. Write each paragraph as one long line and let the editor wrap it; line breaks belong only between paragraphs, list items, and code blocks.

## Commit Messages

Commits follow Conventional Commits with an optional scope, on a single line.

```
feat(cli): add --older-than flag
fix(db): close connection before vacuum
test(prune): cover cascade delete of orphan parts
```

- Format is `type(scope): subject`, and the scope may be omitted.
- Single line only. No body, no footer, no trailer, no co-authorship line.
- Allowed types are `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `chore`, `build`, `ci`, `style`, and `revert`.
- Plain prose subjects, multi-line messages, and emoji are rejected.

The type is not cosmetic. release-please derives the version bump and the changelog from it: `feat` and `fix` produce visible changelog sections, while `test`, `build`, `ci`, `chore`, and `style` are hidden. A wrong type produces a wrong release.

## Pull Requests

Work on a feature branch and open a pull request against `main`.

```sh
git switch -c fix/vacuum-headroom
task ci
git push -u origin fix/vacuum-headroom
```

Before requesting review:

1. `task ci` passes locally.
2. New behavior has tests, and behavior that changed has updated tests.
3. README, `docs/json-report.md`, and this file are updated when the change touches flags, exit codes, or the JSON contract.
4. Every commit message follows the format above, since they become the changelog.

Keep a pull request focused on one concern. A bug fix and an unrelated refactor belong in separate branches.

## Continuous Integration

`.github/workflows/ci.yml` runs three jobs on every push and pull request.

- **Test** runs a native matrix across Ubuntu, macOS, and Windows, executing fmt, clippy, build, nextest, and doctests. The matrix is native rather than cross-compiled because holder detection has a separate implementation per platform: `/proc` on Linux, `libproc` on macOS, and Restart Manager on Windows, and only a native host exercises them.
- **Coverage** enforces the 80 percent line floor on Ubuntu.
- **Dependency advisories** runs `cargo audit --deny warnings`, so a newly published advisory can fail CI even when no code changed.

`task ci` reproduces the fmt, lint, and test portion locally.

## Releases

Releases are automated by release-please. Merging conventional commits to `main` keeps a release pull request up to date with the next version and the generated `CHANGELOG.md`; merging that pull request creates the tag and the GitHub release. Publishing to crates.io and uploading binary artifacts are deliberately not wired up yet.

Do not bump the version in `Cargo.toml` or edit `CHANGELOG.md` by hand.

## Dependencies

Any crate is allowed in principle, but favor stable, widely adopted ones. Recently released crates are acceptable when they are clearly maintained; pre-1.0 crates with heavy churn or no maintenance are not.

The binary is fully synchronous and has no async runtime. `tokio`, `anyhow`, `dirs`, and `ratatui` are excluded by design; propose an alternative before reaching for any of them.

The established choices are `clap` for the CLI, `rusqlite` for SQLite, `thiserror` for the error enum, `tracing` with `tracing-subscriber` for diagnostics, `indicatif` for progress reporting, `ctrlc` for interrupt handling, `fs2` for free-space checks, and `rustix` for openat-based traversal on Unix. JSON is emitted through `serde_json` values built by hand, so there is no `serde` derive dependency. Holder detection dependencies are target-gated: `procfs` on Linux, `libproc` on macOS, and `windows` on Windows.

Adding a dependency means a new advisory surface for the audit job, so justify it in the pull request description.

## Code Style

`cargo fmt` settles formatting; do not hand-format around it. Clippy runs with the `all` and `pedantic` groups promoted to warnings in `Cargo.toml`, and CI denies warnings, so a lint must be fixed or given a narrowly scoped `#[allow]` with a `reason`.

`unsafe_code` is set to `warn`. All existing unsafe code sits in the Windows platform layer, `src/safety/holders/windows.rs` and `src/reclaim/platform/windows.rs`, where it calls Restart Manager and file APIs through FFI. Every unsafe block carries a `SAFETY` comment explaining the invariant it upholds, and a new one must do the same.

Comments are written in English and reserved for non-obvious logic, algorithms, and surprising behavior. Self-explanatory code does not need narration.

## License

By contributing, you agree that your contributions are dual licensed under the [Apache License, Version 2.0](LICENSE-APACHE) and the [MIT License](LICENSE-MIT), matching the license of the project itself.
