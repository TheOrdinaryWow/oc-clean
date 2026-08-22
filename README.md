# oc-clean

English | [Simplified Chinese](README.zh.md)

`oc-clean` is a Rust command-line utility for inspecting, pruning, and compacting OpenCode's SQLite database and its associated storage. It provides read-only analysis and diagnostics, dry-run cleanup planning, bounded session deletion, orphan cleanup, snapshot garbage collection, and database space reclamation.

## Why This Exists

OpenCode stores sessions, messages, parts, events, and related state in SQLite. Sessions have no automatic expiration, OpenCode does not run `VACUUM`, and `event` rows are left behind when sessions disappear because those rows do not cascade from session deletion. The main database can therefore grow without bound and reach tens of gigabytes during normal long-term use.

Deleting rows alone places pages on SQLite's freelist; it does not normally shrink the database file. `oc-clean` deletes the selected relational data and can rebuild the database with `VACUUM INTO`, or use incremental vacuum when the database was already configured for `auto_vacuum=INCREMENTAL`.

> `clean` and `vacuum` mutate the database. Each one prints its impact and waits for an interactive `yes` before doing anything; `--dry-run` prints the same report and exits instead. Keep the generated backup until the result has been independently checked, and stop OpenCode before destructive work.

## OpenCode Version Compatibility

`oc-clean` is bound to one OpenCode schema. It targets **OpenCode 1.18.19**, whose schema is extracted from the release binary and committed as `src/db/opencode_schema.sql`; that file's header records the source version, its sha256, and the extraction commands. Every SQL statement the tool issues names those tables and columns directly, so the binding is real rather than nominal.

The binding is not exact-version equality. OpenCode releases that leave the tables and typed columns `oc-clean` reads unchanged will work, and OpenCode's own schema differs between a fresh install and an upgraded database, so both shapes are supported. What matters is whether the objects the tool depends on are still present and still typed the same way.

`doctor` answers that question without touching data, and it is the first thing to run after an OpenCode upgrade:

```sh
oc-clean doctor
```

Its `Schema Compatibility` section reports all three tiers described under [Schema Strictness](#schema-strictness), and it is deliberately more permissive than the other commands: only a Tier 1 failure stops it, so a database the other commands refuse can still be diagnosed.

| What `doctor` reports | `doctor` | `analyze`, `clean`, `vacuum` | What it means |
|---|---|---|---|
| Tier 1 required — findings | exit 4 | exit 4 | A table or typed column the SQL requires is missing or has an incompatible type. No flag bypasses it, including `--force-schema`. |
| Tier 2 extensions — findings | exit 0 | exit 0 | Unknown tables, columns, or indexes the tool never reads. This is the normal shape of an OpenCode release that added something. |
| Tier 3 semantics — findings | exit 0 | exit 4 | A foreign key, trigger, or view exists that the verified contract does not describe, so deletion semantics may differ from what was measured. `--force-schema` downgrades it to a warning. |

A Tier 1 failure names the exact object, for example `missing required column session.time_archived`. Until multi-version support exists, treat it as "this build does not support that OpenCode" rather than something to work around: use an `oc-clean` release built for that OpenCode version, or wait for one. `analyze` and `doctor` are read-only in every case, so diagnosing an unknown database is always safe.

## Installation From Source

The project uses Rust edition 2024, declares Rust 1.85 as its minimum version, and develops against nightly. `rust-toolchain.toml` pins that toolchain, so rustup selects and installs it automatically for commands run inside the checkout. Install the binary from a checked-out source tree with:

```sh
cargo install --locked --path .
oc-clean --help
```

The optimized binary is installed as `oc-clean` in Cargo's binary directory, normally `~/.cargo/bin`.

## Database Selection

The effective database precedence is an explicit `--db PATH`, then `OCC_DB`, then OpenCode's existing `OPENCODE_DB`, then the platform default. `OCC_DB` is the clap environment binding equivalent to `--db`; `OPENCODE_DB` remains OpenCode's own database selector and may be absolute or relative to the OpenCode data directory. The special value `:memory:` creates a fresh in-memory OpenCode schema for `analyze` and `doctor`. Destructive commands require a file-backed database.

Without an override, Linux and macOS resolve the latest channel to `~/.local/share/opencode/opencode.db`, subject to `XDG_DATA_HOME`; Windows uses the corresponding OpenCode data directory under `USERPROFILE`. `--channel NAME` or `OCC_CHANNEL` selects `opencode-<NAME>.db` for custom channels when no database-path override is present; `latest`, `beta`, and `prod` continue to use `opencode.db`. The derived external paths are sibling `storage`, `snapshot`, `tool-output`, and `log` directories.

```sh
oc-clean --db /srv/opencode/opencode.db analyze
OCC_DB=/srv/opencode/opencode.db oc-clean doctor
oc-clean analyze --db :memory: --json
OCC_CHANNEL=nightly oc-clean analyze --quick
OPENCODE_DB=opencode-beta.db oc-clean analyze --quick
```

## Quick Start

Run a full report, diagnose safety conditions, preview a conservative cleanup with `--dry-run`, and then repeat the exact command without it to confirm and execute:

```sh
oc-clean analyze
oc-clean doctor
oc-clean clean --older-than 90D --dry-run
oc-clean clean --older-than 90D
```

The second command prints the same impact, asks `Proceed? [y/n]`, and deletes only after `y` or `yes`. Answering `n` or `no` cancels. An answer that is neither is re-asked, up to three attempts per question. Automation must add `--dangerously-skip-confirm` explicitly after reviewing the same `--dry-run` selection.

## Commands

### `analyze`

`analyze` opens the database read-only. Full mode reports database allocation, table and row distribution, session age and size distributions, orphan counts, largest sessions, project rollups, and associated external storage. Quick mode limits work to file-level accounting and row counts.

Each reported session carries its title, owning project path, last-activity date, and message count alongside its size, because a session identifier is a random string that tells an operator nothing about what the session contains. The project rollup is keyed by absolute worktree path for the same reason, with the project identifier kept beside it. A path too long for the terminal is shortened from the front, so the trailing directories that distinguish one checkout from another stay visible. Those descriptions are looked up only for the sessions the report displays, so `--top` bounds their cost.

```sh
oc-clean analyze
oc-clean analyze --top 25
oc-clean analyze --quick --json
oc-clean analyze --json --log json
```

### `doctor`

`doctor` opens the database read-only and reports schema compatibility, SQLite integrity, foreign-key integrity, orphan census, current holder scan, rebuild headroom, auto-vacuum mode, and timestamp-unit sanity. A failed integrity or timestamp check produces exit code 7.

```sh
oc-clean doctor
oc-clean doctor --json
OCC_DB=/var/lib/opencode/opencode.db oc-clean doctor
```

### `clean`

`clean` requires at least one selector: `--older-than`, `--include`, `--exclude`, `--larger-than`, `--archived`, or `--orphans`. Multiple session predicates are intersected and subtree selection preserves parent-child consistency. `--include` and `--exclude` are two directions of the same project-path predicate and cannot be combined; supplying both is a usage error. `--keep-recent N` protects the `N` most recently active root sessions per project, counted per project against root sessions rather than against every session; it retains nothing by default, so the selection is exactly what the selectors describe.

```sh
# Preview root session subtrees inactive for at least 90 days.
oc-clean clean --older-than 90D --dry-run

# Preview archived subtrees larger than 250 decimal megabytes.
oc-clean clean --archived --larger-than 250MB --dry-run

# Preview all matching sessions for project paths selected by the glob.
oc-clean clean --include '/work/legacy-*' --keep-recent 20 --dry-run

# Preview everything outside one project, which is the complement of the same glob.
oc-clean clean --exclude '/work/keep-this' --older-than 30D --dry-run

# Preview session-shaped orphan events, dangling sessions, and external orphans.
oc-clean clean --orphans --dry-run

# Delete the reviewed selection, confirming interactively.
oc-clean clean --older-than 6M --gc-snapshots

# Run from a non-interactive job after an equivalent dry run was reviewed.
oc-clean clean --older-than 1Y --dangerously-skip-confirm --json
```

The impact report lists the largest selected sessions with their titles and owning project paths before any deletion, capped by `--top`, so a selection can be recognized rather than only counted.

Confirmed cleanup deletes sessions in bounded transactions of 2,500 candidates, deletes matching event aggregates explicitly, prunes affected empty projects, removes corresponding storage and snapshot artifacts, runs integrity checks, and rebuilds the database by default. `--no-vacuum` commits deletion while leaving free pages in the database file. `--incremental` uses incremental auto-vacuum and requires the source database to have been configured and rebuilt previously with `auto_vacuum=INCREMENTAL`. `--gc-snapshots` also compacts retained snapshot repositories.

### `vacuum`

`vacuum` reclaims existing SQLite freelist space without selecting or deleting application rows. Default mode uses a verified `VACUUM INTO` rebuild and atomic swap; `--incremental` requests bounded incremental-vacuum batches on a database already using incremental auto-vacuum. Like `clean`, it prints its report and requires an interactive `yes`, and `--dry-run` stops after the report.

```sh
# Preview rebuild strategy, estimated compacted size, and required headroom.
oc-clean vacuum --dry-run

# Rebuild, confirm interactively, and retain a timestamped backup.
oc-clean vacuum

# Preview and then run incremental reclamation where supported by the database.
oc-clean vacuum --incremental --dry-run
oc-clean vacuum --incremental
```

## Options

The table is the complete set of long flags defined by the current clap interface. `Global` flags are accepted before or after every subcommand; each command-specific row is scoped to the named subcommand.

| Scope | Flag | Environment | Purpose |
|---|---|---|---|
| Global | `--version` | CLI only | Print the version recorded in `Cargo.toml` and exit. `-V` is the short form. |
| Global | `--db <PATH>` | `OCC_DB` | Select the database path or `:memory:` for a fresh in-memory analysis target. This takes precedence over `OPENCODE_DB` and platform discovery. |
| Global | `--channel <NAME>` | `OCC_CHANNEL` | Select the OpenCode channel used for the discovered database filename. |
| Global | `--log <MODE>` | `OCC_LOG` | Select stderr diagnostics: `off`, `text`, or `json`; default is `off`. Setting `RUST_LOG` implicitly selects `text`. |
| Global | `--dry-run` | `OCC_DRY_RUN` | Print the `clean` or `vacuum` report and exit without mutating anything or prompting. |
| Global | `--force` | CLI only | Downgrade a held or indeterminate holder gate to a warning for applied `clean` or `vacuum`. |
| Global | `--force-schema` | CLI only | Downgrade Tier 3 schema-semantic findings to warnings; Tier 1 remains mandatory and Tier 2 is already tolerated. |
| Global | `--dangerously-skip-confirm` | CLI only | Bypass the interactive confirmation required by applied `clean` and `vacuum`, including JSON and piped execution. |
| Global | `--skip-backup` | CLI only | Remove the temporary rollback copy after a successful full rebuild instead of retaining the default `.bak` file. |
| analyze | `--json` | `OCC_JSON` | Emit one stable JSON report on stdout. |
| analyze | `--top <N>` | `OCC_TOP` | Limit the largest-session rollup; default is `10`. |
| analyze | `--quick` | `OCC_QUICK` | Emit file accounting and row counts without full distributions and rollups. |
| doctor | `--json` | `OCC_JSON` | Emit one stable JSON diagnostic report on stdout. |
| clean | `--older-than <AGE>` | `OCC_OLDER_THAN` | Select session subtrees whose latest activity is at least this coarse age. |
| clean | `--include <PATH_OR_GLOB>` | `OCC_INCLUDE` | Select sessions belonging to a matching project path or glob. Conflicts with `--exclude`. |
| clean | `--exclude <PATH_OR_GLOB>` | `OCC_EXCLUDE` | Select sessions whose project path or glob does not match. Conflicts with `--include`. |
| clean | `--larger-than <SIZE>` | `OCC_LARGER_THAN` | Select session subtrees whose attributable payload reaches this decimal size. |
| clean | `--archived` | `OCC_ARCHIVED` | Select archived sessions. |
| clean | `--orphans` | `OCC_ORPHANS` | Include session-shaped orphan events, dangling sessions, and orphan external storage. |
| clean | `--keep-recent <N>` | `OCC_KEEP_RECENT` | Retain this many most recently active root sessions per project; default is `0`, which retains nothing. |
| clean | `--incremental` | `OCC_INCREMENTAL` | Reclaim free pages with incremental vacuum on a database already using `auto_vacuum=INCREMENTAL`. |
| clean | `--no-vacuum` | `OCC_NO_VACUUM` | Commit selected deletion and skip page reclamation. |
| clean | `--gc-snapshots` | `OCC_GC_SNAPSHOTS` | Compact retained snapshot repositories after cleanup. |
| clean | `--prune-empty-projects` | `OCC_PRUNE_EMPTY_PROJECTS` | Also prune projects that were already empty before cleanup. |
| clean | `--top <N>` | `OCC_TOP` | Limit the selected-session preview listed before deletion; default is `10`. |
| clean | `--json` | `OCC_JSON` | Emit the cleanup report as JSON on stdout. |
| vacuum | `--json` | `OCC_JSON` | Emit the vacuum report as JSON on stdout. |
| vacuum | `--incremental` | `OCC_INCREMENTAL` | Reclaim freelist pages with incremental vacuum instead of a full rebuild. |

## Environment Variables

The executable is named `oc-clean`, while its own environment-variable prefix is `OCC_`. The current clap interface exposes an `OCC_*` environment binding for every non-destructive option across all four subcommands; destructive switches intentionally require visible command-line input.

`OCC_CHANNEL` is the environment equivalent of `--channel`; explicit `--db`/`OCC_DB` and `OPENCODE_DB` path selectors take precedence over channel naming. `OPENCODE_DB` belongs to OpenCode and participates in fallback database discovery after `--db` and `OCC_DB`. `XDG_DATA_HOME`, `HOME`, `USERPROFILE`, and `OPENCODE_DISABLE_CHANNEL_DB` may also influence platform discovery. `NO_COLOR` disables color in human report output. `RUST_LOG` selects the tracing filter and, when `--log`/`OCC_LOG` is unset, implicitly enables `text` diagnostics.

## Duration And Size Grammars

`--older-than` accepts `<integer><unit>` with `D`, `W`, `M`, or `Y`. `D` and `W` are case-insensitive; `M` is uppercase because `M` means a fixed 30-day month and lowercase `m` would imply minutes; `Y` means a fixed 365-day year. Examples are `30D`, `12w`, `6M`, and `1Y`. Values use checked integer arithmetic and reject negatives, fractions, missing units, non-ASCII digits, unsupported units, overflow, and all sub-day units such as hours, minutes, and seconds.

`--larger-than` accepts `<number><unit>` with case-insensitive `MB` or `GB`. The units are decimal SI: `1MB = 1,000,000 bytes` and `1GB = 1,000,000,000 bytes`. Decimal fractions are accepted when they resolve to a whole number of bytes, such as `1.5GB`; values reject negatives, missing units, non-ASCII digits, malformed fractions, overflow, and binary units such as `MiB` and `GiB`.

Sub-day duration units and binary size units are explicitly rejected. Incremental vacuum's internal page batches are unrelated to these user-facing grammars.

`--include` and `--exclude` accept a literal path or a glob. Glob mode activates when the value contains `*`, `?`, or `[`: `*` matches any sequence including separators, `?` matches one character, `[abc]` and `[a-z]` match a character class, and a leading `!` inside the brackets negates it. Trailing separators are stripped before matching, so `/work/repo/` and `/work/repo` are the same pattern. Matching follows the platform's filesystem case policy, so it is case-insensitive on macOS and Windows and case-sensitive on Linux. A pattern is compared against each project's worktree and against every directory registered for that project, and matching either one selects all of that project's sessions regardless of each session's own working directory.

## Safety Model

### Dry Runs And Confirmation

`analyze` and `doctor` are read-only. `clean` and `vacuum` mutate, and both stop at an interactive confirmation that displays the full impact. `y` and `yes` proceed, `n` and `no` cancel, and case and surrounding whitespace are ignored. Any other answer is re-asked; each question allows three attempts before the command gives up. Canceling is reported as a plain notice rather than an `error:` line, because declining is a decision, and it still exits with code 2 so a script can tell that nothing ran. `--dry-run` performs the same selection, compatibility, holder, and headroom work, prints the report, and exits while preserving database bytes. Mutating runs acquire SQLite's exclusive lock before touching data. Piped input, JSON output, or a missing terminal cannot answer the prompt and therefore refuse with exit code 2 unless `--dangerously-skip-confirm` is supplied.

A `clean` selecting at least half of the sessions in the database asks a second, independent question after the first, naming the selected count against the database total; both answers must be affirmative. The second question carries its own attempt allowance. `--dangerously-skip-confirm` bypasses both prompts, and leaves holder, schema, lock, headroom, and integrity gates active. A mistaken selector or database path can therefore execute unattended and delete the wrong data.

### Holder Detection

Before cleanup or reclamation, the platform inspector scans the database, WAL, and SHM paths and reports one of `CompleteForVisibleProcesses`, `PartialDueToPermissions`, or `Unsupported`. Linux uses visible `/proc` file descriptors, macOS uses `libproc`, and Windows uses Restart Manager. The scan is a point-in-time observation, can see only processes available through the current account and platform APIs, and cannot prevent another process from connecting after the scan. `CompleteForVisibleProcesses` covers visible processes only; it does not prove database quiescence.

Applied `clean` and `vacuum` refuse observed holders, and refuse an indeterminate scan only when the platform could not scan at all (`Unsupported`). A scan reported as `PartialDueToPermissions` still covered every process the account can see, so it warns and proceeds: an unprivileged account can never read another user's descriptor table, and refusing on that basis alone would block every non-root invocation without establishing anything. `--force` bypasses the remaining preliminary refusals, changing them to warnings; SQLite lock acquisition, `data_version` checks, schema policy, disk headroom, confirmation, and integrity checks remain active. Forcing against a live process can disrupt OpenCode, race external file cleanup, or leave writes attached to handles for the replaced database inode.

### Schema Strictness

Tier 1 contains tables and typed columns required by the SQL implementation. Missing or type-incompatible Tier 1 objects always stop commands and cannot be bypassed. Tier 2 contains tolerated extensions such as unknown indexes and produces warnings. Tier 3 contains foreign keys, triggers, and views that may alter deletion semantics and stops compatibility-enforcing commands by default.

`--force-schema` downgrades Tier 3 findings to warnings only. It never bypasses Tier 1 or changes Tier 2 treatment. Continuing through an unfamiliar trigger, view, or foreign key can delete additional rows, preserve rows the tool expected to remove, or otherwise execute semantics outside the verified schema contract.

### Disk, Integrity, And Interrupts

Full rebuilds perform a free-space headroom check before destructive work; insufficient space returns exit code 6 in interactive and non-interactive modes. The check includes projected live data, one bounded delete-batch WAL allowance where applicable, backup-copy fallback, and a 10 percent margin. `--force`, `--force-schema`, and `--dangerously-skip-confirm` do not bypass this gate. Incremental vacuum has no second full database and structurally follows its own applicability check.

Cleanup uses bounded transactions, runs SQLite integrity and foreign-key checks after relational deletion, verifies rebuilt output before swap, and verifies the installed database afterward. An interrupt before mutation leaves zero mutations; an interrupt during deletion stops after the current committed batch and reports exit code 8 with completed work. Rebuild swap failures attempt rollback, and exit code 11 means both swap verification and rollback failed, requiring manual recovery from the named paths.

## Backup And Peak Disk Space

A successful default full rebuild retains the original database as a sibling named `opencode.db.bak.YYYYMMDDTHHMMSSZ`. The `.bak` file is never auto-deleted; backup retention and deletion belong to the operator. Incremental vacuum and `clean --no-vacuum` do not create this rebuild backup.

Let `O` be the original database size, `L` the projected post-delete live bytes, `W` one delete-batch WAL allowance, and `margin = ceil(0.10 * L)`. With hard-link support, additional free space is `L + W + margin`, so peak filesystem use is approximately `O + L + W + margin`; the retained `.bak` and original path initially share the same inode. When hard links are unavailable, the backup falls back to a full copy and additional free space is `L + O + W + margin`, making peak use approximately `2O + L + W + margin`. Standalone `vacuum` uses `W = 0` and its current live-byte estimate for `L`.

`--skip-backup` treats backup-copy bytes as zero for headroom and removes the temporary rollback copy after successful verification. A later problem then has no retained original database from this run, so recovery depends on an independent backup.

## Output And Automation

Human reports go to stdout and everything else goes to stderr. `--json` emits one stable report object on stdout.

Diagnostics are off by default, so a normal run's stderr stays empty and progress bars are the only thing drawn there. `--log text` or `--log json` turns tracing diagnostics on; setting `RUST_LOG` selects `text` implicitly. Progress rendering is suppressed whenever `--json` is used or stderr is not a terminal, so a redirected run captures no redraw sequences.

Failures never depend on `--log`. A failed command writes an `error:` line to stderr, plus a `hint:` line when a specific next step applies; under `--json` it writes one parsable failure object to stderr instead, carrying `kind`, `exit_code`, `message`, and an optional `hint`. See [docs/json-report.md](docs/json-report.md) for the field contract.

```sh
oc-clean analyze --json > analysis.json
oc-clean clean --older-than 120D --dry-run --json > preview.json
oc-clean clean --older-than 120D --dangerously-skip-confirm --json > result.json
```

## Measured Performance

The committed benchmark uses a generated 2,000,000,000-byte target fixture that reached 1,962,291,200 bytes with 2,675 sessions, 53,500 messages, and 214,000 parts. The recorded full analysis took 1,687.796 ms cold and 1,576.023 ms warm; quick analysis took 8.662 ms. These are measured regression references from `benchmarks.json`, and host hardware, filesystem, SQLite behavior, retained data shape, and cache state affect local results.

Delete-batch tuning on the same 2,675-session fixture produced:

| Candidate batch size | Elapsed time | Transactions |
|---|---:|---:|
| 1,000 | 3,198.648 ms | 3 |
| 2,500 | 2,934.798 ms | 2 |
| 5,000 | 3,035.092 ms | 1 |
| 10,000 | 3,187.325 ms | 1 |

The 2,500-candidate batch was fastest in this measurement and is the current deletion default. Reproduce the large fixture regression locally with:

```sh
cargo test --release --features bench-large --test perf performance_budgets_hold_on_the_committed_large_fixture_scale -- --ignored --nocapture --test-threads=1
```

## Exit Codes

Exit codes are a stable process contract. Several error variants intentionally share a category code.

| Code | Error variant | Meaning |
|---:|---|---|
| 0 | `Success` | Complete success. |
| 1 | `Io` | Filesystem, terminal, or output I/O failure. |
| 1 | `Sqlite` | Generic SQLite failure outside a more specific category. |
| 2 | `InvalidArgument` | Invalid value, missing selector, or incompatible invocation. |
| 2 | `Canceled` | The operator declined the confirmation, or never answered it. |
| 3 | `NotFound` | Database file does not exist. |
| 4 | `SchemaIncompatible` | Required schema is missing or deletion semantics are unrecognized. |
| 5 | `DatabaseBusy` | Holder policy, SQLite locking, or concurrent-change protection refused the operation. |
| 6 | `InsufficientDiskSpace` | Full rebuild headroom is below the calculated requirement. |
| 6 | `ReclaimUnavailable` | Requested reclaim strategy is unavailable, including an incremental-vacuum precondition failure. |
| 7 | `IntegrityCheckFailed` | SQLite integrity, foreign-key integrity, or timestamp sanity failed. |
| 8 | `Interrupted` | SIGINT stopped the operation; the message states completed work. |
| 9 | `UnsupportedPlatform` | Platform has no supported implementation. |
| 10 | `PartialSuccess` | Primary work completed while some external cleanup was left behind. |
| 11 | `SwapRollbackFailed` | Database swap verification failed and rollback also failed; manual recovery is required. |

## What This Tool Will NOT Do

- Automatic scheduling, background retention, and daemon operation remain outside the command-line utility; operators decide when to run it.
- Database quiescence remains an operator responsibility; holder detection provides a point-in-time, visibility-limited safety signal.
- Incremental vacuum remains a bounded freelist-reclamation strategy for databases already configured for it; a full rebuild provides separate compaction and verification behavior.
- Schema migration remains outside scope; incompatible Tier 1 databases require a compatible release or a separately reviewed migration.
- Supporting several OpenCode schema versions from one binary remains roadmap work; a given build targets the single version named under [OpenCode Version Compatibility](#opencode-version-compatibility).
- Backup lifecycle management remains outside scope; default `.bak` files persist until the operator removes them.
- Live OpenCode process shutdown remains outside scope; stop OpenCode before applied cleanup or reclamation.
- AFT and Magic Context cleanup integrations remain roadmap work.

## Roadmap

- Multi-version OpenCode support is wanted but not built. One build currently targets one schema, so an OpenCode release that changes a table or column `oc-clean` reads requires a matching `oc-clean` release. Supporting several schema revisions from a single binary would remove that coupling.
- AFT cleanup integration is wanted but not built.
- Magic Context cleanup integration is wanted but not built.

## Development

The default quality gates use the pinned nightly toolchain, rustfmt, Clippy's configured `all` and `pedantic` lint groups, cargo-nextest, and line coverage of at least 80 percent. `Taskfile.yml` wraps them; run `task` with no arguments to list every target.

```sh
task ci         # fmt:check, then lint, then test
task test       # cargo nextest run --no-tests=pass
task test:doc   # doctests, which nextest does not run
task coverage   # line coverage report against the 80 percent floor
task lint       # clippy --all-targets --all-features -- -D warnings
task audit      # cargo audit dependency advisory scan
```

The equivalent direct cargo invocations are:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --no-tests=pass
cargo test --doc
cargo llvm-cov --all-features nextest --no-tests=pass --fail-under-lines 80
```

Tests create disposable SQLite fixtures under temporary directories. Development and testing must never target a live OpenCode database.

Continuous integration runs the fmt, lint, build, test, and doctest sequence natively on Ubuntu, macOS, and Windows, because holder detection has a distinct implementation per platform. Separate jobs enforce the coverage floor and scan dependencies for advisories.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the commit format, the branch and pull request workflow, and release automation.

## License

Licensed under either the [Apache License, Version 2.0](LICENSE-APACHE) or the [MIT License](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in `oc-clean`, as defined in the Apache-2.0 license, is dual licensed under these terms without additional conditions.
