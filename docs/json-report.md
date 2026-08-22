# JSON report contract

`oc-clean analyze --json` writes exactly one JSON object to stdout. Diagnostics remain on stderr. The top-level `schema_version` integer is the compatibility boundary for downstream consumers; additive fields may appear within a schema version, while removals, renames, and semantic changes require a version increment.

The schema version 1 object contains these keys:

- `schema_version`: integer, currently `1`.
- `mode`: `"full"` or `"quick"`.
- `file_space`: SQLite page counts, page size, live/freelist bytes and percentage, plus optional WAL and SHM bytes.
- `row_counts`: object mapping every application table name to its row count.
- `table_space`: accounting method, accuracy label, and table/index/schema byte entries.
- `project_attribution`: project identifiers and attributed payload bytes.
- `largest_sessions`: session and project identifiers with self and descendant-subtree payload bytes, plus the optional description fields below.
- `orphans`: count and estimated bytes for all five orphan classes.
- `age_distribution`: fixed age ranges with session counts and payload bytes.
- `external_directories`: recursive file counts and byte totals for storage, snapshot, tool-output, and log directories.

Full mode populates all seven analysis layers. Quick mode populates `file_space` and `row_counts`, and represents every scan-backed layer as JSON `null`. Byte quantities are unsigned integers in bytes; percentages are JSON numbers.

## Session description fields

Each `largest_sessions` entry always carries `session_id`, `project_id`, `self_bytes`, and `subtree_bytes`. These additional fields are additive within schema version 1 and describe the session for a human reader:

- `title`: string, the session title recorded by OpenCode.
- `time_updated`: integer, the session's last activity as a millisecond epoch timestamp.
- `message_count`: integer, the session's message count. OpenCode stores messages under two coexisting models, so this is the larger of the legacy `message` count and the event-sourced `session_message` count rather than their sum.
- `project_path`: string, the absolute worktree path of the owning project. `project_id` is a hash and cannot identify a checkout on its own, so this is the field to display or group by.

The first three are omitted together when the session row could not be read, which happens if the row disappeared between the size rollup and the description lookup. `project_path` is omitted in that case too, and additionally when the project row named by `project_id` no longer exists: `PRAGMA foreign_keys` is per-connection in SQLite, so a writer that left it off can delete a project without cascading to its sessions. Consumers must treat all four as optional.

## Cleanup report

`oc-clean clean --json` writes its own object whose `impact` member mirrors the dry-run summary. `impact.preview` is an array of the largest selected sessions, capped by `--top`, using the same entry shape and the same optional description fields as `largest_sessions`.

`impact.total_sessions` counts the selection and `impact.database_sessions` counts every session in the database, so a consumer can compute the selection's share without a second query. An interactive run requires a second confirmation when `total_sessions * 2 >= database_sessions`.

## Failure objects

A failed command writes one JSON object to **stderr** when `--json` is in effect, leaving stdout carrying either exactly one report object or nothing at all. The failure object contains:

- `error`: boolean, always `true`, so a failure is distinguishable from a report.
- `kind`: stable snake-case identifier for the failure category, such as `not_found`, `database_busy`, or `canceled`. It changes only alongside the documented exit-code table. A `canceled` object reports an operator who declined or never answered a confirmation, which is a decision rather than a fault even though it exits non-zero.
- `exit_code`: integer, the same value the process exits with.
- `message`: human-readable description of the failure.
- `hint`: optional string naming the next action an operator can take. It is absent when no specific next step applies.

A human-format failure writes the same information to stderr as an `error:` line plus an optional `hint:` line. Failures never depend on `--log` being enabled.
