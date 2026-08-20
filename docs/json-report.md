# JSON report contract

`oc-clean analyze --json` writes exactly one JSON object to stdout. Diagnostics remain on stderr. The top-level `schema_version` integer is the compatibility boundary for downstream consumers; additive fields may appear within a schema version, while removals, renames, and semantic changes require a version increment.

The schema version 1 object contains these keys:

- `schema_version`: integer, currently `1`.
- `mode`: `"full"` or `"quick"`.
- `file_space`: SQLite page counts, page size, live/freelist bytes and percentage, plus optional WAL and SHM bytes.
- `row_counts`: object mapping every application table name to its row count.
- `table_space`: accounting method, accuracy label, and table/index/schema byte entries.
- `project_attribution`: project identifiers and attributed payload bytes.
- `largest_sessions`: session and project identifiers with self and descendant-subtree payload bytes.
- `orphans`: count and estimated bytes for all five orphan classes.
- `age_distribution`: fixed age ranges with session counts and payload bytes.
- `external_directories`: recursive file counts and byte totals for storage, snapshot, tool-output, and log directories.

Full mode populates all seven analysis layers. Quick mode populates `file_space` and `row_counts`, and represents every scan-backed layer as JSON `null`. Byte quantities are unsigned integers in bytes; percentages are JSON numbers.
