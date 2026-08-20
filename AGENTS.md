# AGENTS.md

`oc-clean` — OpenCode Database Cleaner. A Rust CLI that manually prunes OpenCode's SQLite database (sessions/messages/context older than N days) and reclaims disk space. OpenCode never vacuums or expires anything, so real-world databases reach tens of gigabytes.

## Rust skills

This project ships a `rust-skills` package, and it is the authority on how Rust is written here.

**Before writing or reviewing any Rust, load the matching skill and follow it.** This is mandatory, and it applies to sub-agents exactly as it applies to you. Do not fall back on general Rust knowledge when a skill covers the topic, and do not restate or override skill guidance in this file.

## Toolchain

Nightly is the working toolchain.

`Cargo.toml` must keep `edition = "2024"` and `rust-version = "1.85"`, plus the lint block mandated by rust-skills:

```toml
[lints.rust]
unsafe_code = "warn"

[lints.clippy]
all = "warn"
pedantic = "warn"
```

`cargo-nextest` is installed. `cargo-llvm-cov` is not — install it before the first coverage run.

## Dependencies

Any crate is allowed, but favor stable, widely-adopted ones. Recently released crates are acceptable when they are clearly maintained; pre-1.0 churn-heavy or abandoned crates are not.

Preferred baseline: `tokio` (async runtime), `clap` (CLI, derive API), `serde` (serialization). Reach for those before introducing an alternative, and say why when you do.

## Testing

**TDD is the default.** Unless the user gives a different testing strategy for a task, write the failing test first, then the implementation. This applies to sub-agents too.

**Line coverage target: 80%.** Verify before declaring work done:

```sh
cargo nextest run                     # test run
cargo llvm-cov --all-features nextest # coverage report
```

Coverage is a floor, not a goal — do not pad it with tests that assert nothing.

Never point tests at a real OpenCode database. Build throwaway SQLite fixtures in `tempfile` directories; destructive behavior (deletes, `VACUUM`) must be proven against fixtures only.

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

## Repo boundaries

Never commit `.omo/` or `.sisyphus/`.

Do not connect to, read, or modify the user's live OpenCode database (`~/.local/share/opencode/opencode.db`) while developing. Destructive operations against real user data happen only when the user explicitly runs the built CLI.

`.agents/skills/` and `skills-lock.json` are vendored skill sources — edit them only when the task is explicitly about skill maintenance.
