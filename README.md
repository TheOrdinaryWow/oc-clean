# oc-clean

`oc-clean` is a Rust command-line utility for inspecting, pruning, and compacting OpenCode's SQLite database and its associated storage. It provides read-only analysis and diagnostics, dry-run cleanup planning, bounded session deletion, orphan cleanup, snapshot garbage collection, and database space reclamation.

## Why This Exists

OpenCode stores sessions, messages, parts, events, and related state in SQLite. Sessions have no automatic expiration, OpenCode does not run `VACUUM`, and `event` rows are left behind when sessions disappear because those rows do not cascade from session deletion. The main database can therefore grow without bound and reach tens of gigabytes during normal long-term use.

Deleting rows alone places pages on SQLite's freelist; it does not normally shrink the database file. `oc-clean` deletes the selected relational data and can rebuild the database with `VACUUM INTO`, or use incremental vacuum when the database was already configured for `auto_vacuum=INCREMENTAL`.

> `clean` and `vacuum` are dry runs by default. Review their reports before adding `--apply`, keep the generated backup until the result has been independently checked, and stop OpenCode before destructive work.

## Installation From Source

The project uses Rust edition 2024, declares Rust 1.85 as its minimum version, and develops against the nightly toolchain. Install the binary from a checked-out source tree with:

```sh
rustup toolchain install nightly
rustup override set nightly
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

Run a full report, diagnose safety conditions, preview a conservative cleanup, and then repeat the exact cleanup with `--apply`:

```sh
oc-clean analyze
oc-clean doctor
oc-clean clean --older-than 90D
oc-clean clean --older-than 90D --apply
```

The applied command requires an interactive `yes` confirmation. Automation must add `--dangerously-skip-confirm` explicitly after reviewing the same dry-run selection.

## Commands

### `analyze`

`analyze` opens the database read-only. Full mode reports database allocation, table and row distribution, session age and size distributions, orphan counts, largest sessions, project rollups, and associated external storage. Quick mode limits work to file-level accounting and row counts.

```sh
oc-clean analyze
oc-clean analyze --top 25
oc-clean analyze --quick --json
oc-clean analyze --json --log-format json
```

### `doctor`

`doctor` opens the database read-only and reports schema compatibility, SQLite integrity, foreign-key integrity, orphan census, current holder scan, rebuild headroom, auto-vacuum mode, and timestamp-unit sanity. A failed integrity or timestamp check produces exit code 7.

```sh
oc-clean doctor
oc-clean doctor --json
OCC_DB=/var/lib/opencode/opencode.db oc-clean doctor
```

### `clean`

`clean` requires at least one selector: `--older-than`, `--project`, `--larger-than`, `--archived`, or `--orphans`. Multiple session predicates are intersected, subtree selection preserves parent-child consistency, and `--keep-recent 100` protects the 100 most recently active root sessions per project by default.

```sh
# Preview root session subtrees inactive for at least 90 days.
oc-clean clean --older-than 90D

# Preview archived subtrees larger than 250 decimal megabytes.
oc-clean clean --archived --larger-than 250MB

# Preview all matching sessions for project paths selected by the glob.
oc-clean clean --project '/work/legacy-*' --keep-recent 20

# Preview session-shaped orphan events, dangling sessions, and external orphans.
oc-clean clean --orphans

# Apply the reviewed selection and confirm interactively.
oc-clean clean --older-than 6M --gc-snapshots --apply

# Apply from a non-interactive job after an equivalent dry run was reviewed.
oc-clean clean --older-than 1Y --apply --dangerously-skip-confirm --json
```

Applied cleanup deletes sessions in bounded transactions of 2,500 candidates, deletes matching event aggregates explicitly, prunes affected empty projects, removes corresponding storage and snapshot artifacts, runs integrity checks, and rebuilds the database by default. `--no-vacuum` commits deletion while leaving free pages in the database file. `--incremental` uses incremental auto-vacuum and requires the source database to have been configured and rebuilt previously with `auto_vacuum=INCREMENTAL`. `--gc-snapshots` also compacts retained snapshot repositories.

### `vacuum`

`vacuum` reclaims existing SQLite freelist space without selecting or deleting application rows. Default mode uses a verified `VACUUM INTO` rebuild and atomic swap; `--incremental` requests bounded incremental-vacuum batches on a database already using incremental auto-vacuum.

```sh
# Preview rebuild strategy, estimated compacted size, and required headroom.
oc-clean vacuum

# Rebuild, confirm interactively, and retain a timestamped backup.
oc-clean vacuum --apply

# Preview and then apply incremental reclamation where supported by the database.
oc-clean vacuum --incremental
oc-clean vacuum --incremental --apply
```

## Options

The table is the complete set of long flags defined by the current clap interface. `Global` flags are accepted before or after every subcommand; each command-specific row is scoped to the named subcommand.

| Scope | Flag | Environment | Purpose |
|---|---|---|---|
| Global | `--db <PATH>` | `OCC_DB` | Select the database path or `:memory:` for a fresh in-memory analysis target. This takes precedence over `OPENCODE_DB` and platform discovery. |
| Global | `--channel <NAME>` | `OCC_CHANNEL` | Select the OpenCode channel used for the discovered database filename. |
| Global | `--apply` | CLI only | Enable mutation for `clean` or `vacuum`; both remain dry runs without it. |
| Global | `--force` | CLI only | Downgrade a held or indeterminate holder gate to a warning for applied `clean` or `vacuum`. |
| Global | `--force-schema` | CLI only | Downgrade Tier 3 schema-semantic findings to warnings; Tier 1 remains mandatory and Tier 2 is already tolerated. |
| Global | `--dangerously-skip-confirm` | CLI only | Bypass the interactive confirmation required by applied `clean` and `vacuum`, including JSON and piped execution. |
| Global | `--skip-backup` | CLI only | Remove the temporary rollback copy after a successful full rebuild instead of retaining the default `.bak` file. |
| analyze | `--json` | `OCC_JSON` | Emit one stable JSON report on stdout. |
| analyze | `--log-format <LOG_FORMAT>` | `OCC_LOG_FORMAT` | Select `text` or `json` diagnostics on stderr; default is `text`. |
| analyze | `--top <N>` | `OCC_TOP` | Limit the largest-session rollup; default is `10`. |
| analyze | `--quick` | `OCC_QUICK` | Emit file accounting and row counts without full distributions and rollups. |
| doctor | `--json` | `OCC_JSON` | Emit one stable JSON diagnostic report on stdout. |
| clean | `--older-than <AGE>` | `OCC_OLDER_THAN` | Select session subtrees whose latest activity is at least this coarse age. |
| clean | `--project <PATH_OR_GLOB>` | `OCC_PROJECT` | Select sessions belonging to an exact project path or glob. |
| clean | `--larger-than <SIZE>` | `OCC_LARGER_THAN` | Select session subtrees whose attributable payload reaches this decimal size. |
| clean | `--archived` | `OCC_ARCHIVED` | Select archived sessions. |
| clean | `--orphans` | `OCC_ORPHANS` | Include session-shaped orphan events, dangling sessions, and orphan external storage. |
| clean | `--keep-recent <N>` | `OCC_KEEP_RECENT` | Retain this many recently active root sessions per project; default is `100`. |
| clean | `--incremental` | `OCC_INCREMENTAL` | Reclaim free pages with incremental vacuum on a database already using `auto_vacuum=INCREMENTAL`. |
| clean | `--no-vacuum` | `OCC_NO_VACUUM` | Commit selected deletion and skip page reclamation. |
| clean | `--gc-snapshots` | `OCC_GC_SNAPSHOTS` | Compact retained snapshot repositories after cleanup. |
| clean | `--prune-empty-projects` | `OCC_PRUNE_EMPTY_PROJECTS` | Also prune projects that were already empty before cleanup. |
| clean | `--json` | `OCC_JSON` | Emit the cleanup report as JSON on stdout. |
| clean | `--log-format <LOG_FORMAT>` | `OCC_LOG_FORMAT` | Select `text` or `json` diagnostics on stderr; default is `text`. |
| vacuum | `--json` | `OCC_JSON` | Emit the vacuum report as JSON on stdout. |
| vacuum | `--log-format <LOG_FORMAT>` | `OCC_LOG_FORMAT` | Select `text` or `json` diagnostics on stderr; default is `text`. |
| vacuum | `--incremental` | `OCC_INCREMENTAL` | Reclaim freelist pages with incremental vacuum instead of a full rebuild. |

## Environment Variables

The executable is named `oc-clean`, while its own environment-variable prefix is `OCC_`. The current clap interface exposes an `OCC_*` environment binding for every non-destructive option across all four subcommands; destructive switches intentionally require visible command-line input.

`OCC_CHANNEL` is the environment equivalent of `--channel`; explicit `--db`/`OCC_DB` and `OPENCODE_DB` path selectors take precedence over channel naming. `OPENCODE_DB` belongs to OpenCode and participates in fallback database discovery after `--db` and `OCC_DB`. `XDG_DATA_HOME`, `HOME`, `USERPROFILE`, and `OPENCODE_DISABLE_CHANNEL_DB` may also influence platform discovery. `NO_COLOR` disables color in human analysis output.

## Duration And Size Grammars

`--older-than` accepts `<integer><unit>` with `D`, `W`, `M`, or `Y`. `D` and `W` are case-insensitive; `M` is uppercase because `M` means a fixed 30-day month and lowercase `m` would imply minutes; `Y` means a fixed 365-day year. Examples are `30D`, `12w`, `6M`, and `1Y`. Values use checked integer arithmetic and reject negatives, fractions, missing units, non-ASCII digits, unsupported units, overflow, and all sub-day units such as hours, minutes, and seconds.

`--larger-than` accepts `<number><unit>` with case-insensitive `MB` or `GB`. The units are decimal SI: `1MB = 1,000,000 bytes` and `1GB = 1,000,000,000 bytes`. Decimal fractions are accepted when they resolve to a whole number of bytes, such as `1.5GB`; values reject negatives, missing units, non-ASCII digits, malformed fractions, overflow, and binary units such as `MiB` and `GiB`.

Sub-day duration units and binary size units are explicitly rejected. Incremental vacuum's internal page batches are unrelated to these user-facing grammars.

## Safety Model

### Dry Runs And Confirmation

`analyze` and `doctor` are read-only. `clean` and `vacuum` are dry runs unless `--apply` is present; their previews perform selection, compatibility, holder, and headroom work while preserving database bytes. Applied commands acquire SQLite's exclusive lock before mutation. Interactive applied commands display impact and accept only `y` or `yes`; piped input, JSON output, or a missing terminal causes refusal unless `--dangerously-skip-confirm` is supplied.

`--dangerously-skip-confirm` bypasses confirmation only. It still requires `--apply` and leaves holder, schema, lock, headroom, and integrity gates active. A mistaken selector or database path can therefore execute unattended and delete the wrong data.

### Holder Detection

Before cleanup or reclamation, the platform inspector scans the database, WAL, and SHM paths and reports one of `CompleteForVisibleProcesses`, `PartialDueToPermissions`, or `Unsupported`. Linux uses visible `/proc` file descriptors, macOS uses `libproc`, and Windows uses Restart Manager. The scan is a point-in-time observation, can see only processes available through the current account and platform APIs, and cannot prevent another process from connecting after the scan. `CompleteForVisibleProcesses` covers visible processes only; it does not prove database quiescence.

Applied `clean` and `vacuum` refuse both observed holders and indeterminate scans. `--force` bypasses those two preliminary refusals only, changing them to warnings; SQLite lock acquisition, `data_version` checks, schema policy, disk headroom, confirmation, and integrity checks remain active. Forcing against a live process can disrupt OpenCode, race external file cleanup, or leave writes attached to handles for the replaced database inode.

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

Human reports go to stdout and diagnostics go to stderr. `--json` emits one stable report object on stdout; `--log-format json` independently changes stderr diagnostics to structured JSON. This separation permits report capture without mixing operational logs.

```sh
oc-clean analyze --json > analysis.json
oc-clean clean --older-than 120D --json > preview.json
oc-clean clean --older-than 120D --apply --dangerously-skip-confirm --json > result.json
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
| 2 | `InvalidArgument` | Invalid value, missing selector, refused confirmation, or incompatible invocation. |
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
- Backup lifecycle management remains outside scope; default `.bak` files persist until the operator removes them.
- Live OpenCode process shutdown remains outside scope; stop OpenCode before applied cleanup or reclamation.
- AFT and Magic Context cleanup integrations remain roadmap work.

## Roadmap

- AFT cleanup integration is wanted but not built.
- Magic Context cleanup integration is wanted but not built.

## Development

The default quality gates use the nightly toolchain, rustfmt, Clippy's configured `all` and `pedantic` lint groups, cargo-nextest, and line coverage of at least 80 percent.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run
cargo llvm-cov --all-features nextest --fail-under-lines 80
```

Tests create disposable SQLite fixtures under temporary directories. Development and testing must never target a live OpenCode database.

## License

Licensed under either the [Apache License, Version 2.0](LICENSE-APACHE) or the [MIT License](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in `oc-clean`, as defined in the Apache-2.0 license, is dual licensed under these terms without additional conditions.
