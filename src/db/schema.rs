//! Runtime compatibility checks for `OpenCode` database schemas.

use std::collections::BTreeSet;

use rusqlite::Connection;
use tracing::warn;

use super::sqlite_error;
use crate::error::Error;

/// A complete three-tier runtime schema compatibility report.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SchemaReport {
    /// Missing or type-incompatible tables and columns required by tool SQL.
    pub tier_one_missing: Vec<String>,
    /// Tolerated schema extensions, including unknown indexes.
    pub tier_two_warnings: Vec<String>,
    /// Foreign keys, triggers, or views that can change delete semantics.
    pub tier_three_findings: Vec<String>,
}

impl SchemaReport {
    /// Returns whether required schema is present and delete semantics are known.
    #[must_use]
    pub fn is_compatible(&self) -> bool {
        self.tier_one_missing.is_empty() && self.tier_three_findings.is_empty()
    }
}

/// Inspects and enforces the three-tier runtime schema policy.
///
/// Tier-one failures always return [`Error::SchemaIncompatible`]. When `force_schema` is true,
/// tier-three findings remain in the returned report and are emitted as warnings. Tier-two
/// extensions are always returned and emitted as warnings.
///
/// # Errors
///
/// Returns [`Error::SchemaIncompatible`] for a tier-one failure or for a tier-three finding when
/// `force_schema` is false. Returns [`Error::Sqlite`] when schema metadata cannot be read.
pub fn inspect(connection: &Connection, force_schema: bool) -> Result<SchemaReport, Error> {
    let report = inspect_report(connection)?;

    for finding in &report.tier_two_warnings {
        warn!(tier = 2, finding, "schema compatibility warning");
    }

    if !report.tier_one_missing.is_empty() {
        return Err(Error::SchemaIncompatible {
            incompatibility: report.tier_one_missing.join("; "),
        });
    }

    if report.tier_three_findings.is_empty() {
        return Ok(report);
    }

    if force_schema {
        for finding in &report.tier_three_findings {
            warn!(tier = 3, finding, "forced schema compatibility warning");
        }
        Ok(report)
    } else {
        Err(Error::SchemaIncompatible {
            incompatibility: report.tier_three_findings.join("; "),
        })
    }
}

/// Collects all three schema tiers without applying the strictness policy.
///
/// This entry point lets report-oriented commands render every discovered tier before deciding
/// how to present incompatibilities.
///
/// # Errors
///
/// Returns [`Error::Sqlite`] when `sqlite_master` or a schema PRAGMA cannot be read.
pub fn inspect_report(connection: &Connection) -> Result<SchemaReport, Error> {
    let objects = schema_objects(connection)?;
    let table_names = objects
        .iter()
        .filter(|object| object.kind == "table")
        .map(|object| object.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut report = SchemaReport::default();

    inspect_required_schema(connection, &table_names, &mut report)?;
    inspect_extensions(connection, &objects, &table_names, &mut report)?;
    inspect_foreign_keys(connection, &table_names, &mut report)?;
    inspect_triggers_and_views(&objects, &mut report);

    report.tier_one_missing.sort();
    report.tier_two_warnings.sort();
    report.tier_three_findings.sort();
    Ok(report)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SchemaObject {
    kind: String,
    name: String,
    table_name: String,
    sql: String,
}

fn schema_objects(connection: &Connection) -> Result<Vec<SchemaObject>, Error> {
    let mut statement = connection
        .prepare(
            "SELECT type, name, tbl_name, COALESCE(sql, '') \
             FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' \
             ORDER BY type, name",
        )
        .map_err(|source| sqlite_error("preparing sqlite_master schema inspection", source))?;
    statement
        .query_map([], |row| {
            Ok(SchemaObject {
                kind: row.get(0)?,
                name: row.get(1)?,
                table_name: row.get(2)?,
                sql: row.get(3)?,
            })
        })
        .map_err(|source| sqlite_error("querying sqlite_master schema objects", source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| sqlite_error("reading sqlite_master schema objects", source))
}

fn inspect_required_schema(
    connection: &Connection,
    table_names: &BTreeSet<&str>,
    report: &mut SchemaReport,
) -> Result<(), Error> {
    for table in REQUIRED_TABLES {
        if !table_names.contains(table.name) {
            report
                .tier_one_missing
                .push(format!("missing required table `{}`", table.name));
            continue;
        }

        let columns = table_columns(connection, table.name)?;
        for required in table.columns {
            match columns.iter().find(|column| column.name == required.name) {
                None => report.tier_one_missing.push(format!(
                    "missing required column `{}.{}`",
                    table.name, required.name
                )),
                Some(column) if affinity(&column.declared_type) != required.affinity => {
                    report.tier_one_missing.push(format!(
                        "required column `{}.{}` has incompatible declared type `{}`; expected {} affinity",
                        table.name,
                        required.name,
                        column.declared_type,
                        required.affinity.name()
                    ));
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}

fn inspect_extensions(
    connection: &Connection,
    objects: &[SchemaObject],
    table_names: &BTreeSet<&str>,
    report: &mut SchemaReport,
) -> Result<(), Error> {
    for table_name in table_names {
        let Some(known_columns) = known_columns(table_name) else {
            report
                .tier_two_warnings
                .push(format!("unknown table `{table_name}`"));
            continue;
        };
        for column in table_columns(connection, table_name)? {
            if !known_columns.contains(&column.name.as_str()) {
                report
                    .tier_two_warnings
                    .push(format!("unknown column `{table_name}.{}`", column.name));
            }
        }
    }

    for object in objects.iter().filter(|object| object.kind == "index") {
        if !KNOWN_INDEXES.contains(&object.name.as_str()) {
            report
                .tier_two_warnings
                .push(format!("unknown index `{}`", object.name));
        }
    }
    Ok(())
}

fn inspect_foreign_keys(
    connection: &Connection,
    table_names: &BTreeSet<&str>,
    report: &mut SchemaReport,
) -> Result<(), Error> {
    for table_name in table_names {
        let mut statement = connection
            .prepare(
                "SELECT `from`, `table`, `to`, on_delete \
                 FROM pragma_foreign_key_list(?1)",
            )
            .map_err(|source| sqlite_error("preparing foreign-key schema inspection", source))?;
        let foreign_keys = statement
            .query_map([table_name], |row| {
                Ok(ForeignKey {
                    source_table: (*table_name).to_owned(),
                    source_column: row.get(0)?,
                    target_table: row.get(1)?,
                    target_column: row.get(2)?,
                    on_delete: row.get(3)?,
                })
            })
            .map_err(|source| {
                sqlite_error(
                    &format!("querying PRAGMA foreign_key_list for `{table_name}`"),
                    source,
                )
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| {
                sqlite_error(
                    &format!("reading PRAGMA foreign_key_list for `{table_name}`"),
                    source,
                )
            })?;

        for foreign_key in foreign_keys {
            if DELETE_TARGETS.contains(&foreign_key.target_table.as_str())
                && !is_known_foreign_key(&foreign_key)
            {
                report.tier_three_findings.push(format!(
                    "unknown foreign key `{}.{}` references delete target `{}.{}` with ON DELETE {}",
                    foreign_key.source_table,
                    foreign_key.source_column,
                    foreign_key.target_table,
                    foreign_key.target_column,
                    foreign_key.on_delete
                ));
            }
        }
    }
    Ok(())
}

fn inspect_triggers_and_views(objects: &[SchemaObject], report: &mut SchemaReport) {
    for object in objects
        .iter()
        .filter(|object| matches!(object.kind.as_str(), "trigger" | "view"))
    {
        let referenced_targets = referenced_delete_targets(object);
        if referenced_targets.is_empty() {
            continue;
        }
        report.tier_three_findings.push(format!(
            "unknown {} `{}` references delete target(s) {}",
            object.kind,
            object.name,
            referenced_targets.join(", ")
        ));
    }
}

fn referenced_delete_targets(object: &SchemaObject) -> Vec<&'static str> {
    let tokens = sql_tokens(&object.sql);
    DELETE_TARGETS
        .iter()
        .copied()
        .filter(|target| {
            (object.kind == "trigger" && object.table_name.eq_ignore_ascii_case(target))
                || tokens.windows(2).any(|pair| {
                    matches!(pair[0].as_str(), "from" | "join" | "update" | "into" | "on")
                        && pair[1] == *target
                })
        })
        .collect()
}

fn sql_tokens(sql: &str) -> Vec<String> {
    sql.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TableColumn {
    name: String,
    declared_type: String,
}

fn table_columns(connection: &Connection, table_name: &str) -> Result<Vec<TableColumn>, Error> {
    let mut statement = connection
        .prepare("SELECT name, type FROM pragma_table_info(?1) ORDER BY cid")
        .map_err(|source| sqlite_error("preparing table-column schema inspection", source))?;
    statement
        .query_map([table_name], |row| {
            Ok(TableColumn {
                name: row.get(0)?,
                declared_type: row.get(1)?,
            })
        })
        .map_err(|source| {
            sqlite_error(
                &format!("querying PRAGMA table_info for `{table_name}`"),
                source,
            )
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| {
            sqlite_error(
                &format!("reading PRAGMA table_info for `{table_name}`"),
                source,
            )
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TypeAffinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

impl TypeAffinity {
    const fn name(self) -> &'static str {
        match self {
            Self::Integer => "INTEGER",
            Self::Text => "TEXT",
            Self::Blob => "BLOB",
            Self::Real => "REAL",
            Self::Numeric => "NUMERIC",
        }
    }
}

fn affinity(declared_type: &str) -> TypeAffinity {
    let declared_type = declared_type.to_ascii_uppercase();
    if declared_type.contains("INT") {
        TypeAffinity::Integer
    } else if ["CHAR", "CLOB", "TEXT"]
        .iter()
        .any(|marker| declared_type.contains(marker))
    {
        TypeAffinity::Text
    } else if declared_type.is_empty() || declared_type.contains("BLOB") {
        TypeAffinity::Blob
    } else if ["REAL", "FLOA", "DOUB"]
        .iter()
        .any(|marker| declared_type.contains(marker))
    {
        TypeAffinity::Real
    } else {
        TypeAffinity::Numeric
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RequiredColumn {
    name: &'static str,
    affinity: TypeAffinity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RequiredTable {
    name: &'static str,
    columns: &'static [RequiredColumn],
}

const fn text(name: &'static str) -> RequiredColumn {
    RequiredColumn {
        name,
        affinity: TypeAffinity::Text,
    }
}

const fn integer(name: &'static str) -> RequiredColumn {
    RequiredColumn {
        name,
        affinity: TypeAffinity::Integer,
    }
}

const REQUIRED_TABLES: &[RequiredTable] = &[
    RequiredTable {
        name: "project",
        columns: &[text("id"), text("worktree")],
    },
    RequiredTable {
        name: "project_directory",
        columns: &[text("project_id"), text("directory")],
    },
    RequiredTable {
        name: "session",
        columns: &[
            text("id"),
            text("project_id"),
            text("parent_id"),
            integer("time_updated"),
            integer("time_archived"),
        ],
    },
    RequiredTable {
        name: "message",
        columns: &[text("id"), text("session_id"), text("data")],
    },
    RequiredTable {
        name: "part",
        columns: &[text("message_id"), text("session_id"), text("data")],
    },
    RequiredTable {
        name: "session_message",
        columns: &[text("session_id"), text("data")],
    },
    RequiredTable {
        name: "session_context_epoch",
        columns: &[text("session_id"), text("baseline"), text("snapshot")],
    },
    RequiredTable {
        name: "event_sequence",
        columns: &[text("aggregate_id")],
    },
    RequiredTable {
        name: "event",
        columns: &[text("aggregate_id"), text("data")],
    },
];

fn known_columns(table_name: &str) -> Option<&'static [&'static str]> {
    known_session_columns(table_name)
        .or_else(|| known_event_columns(table_name))
        .or_else(|| known_control_columns(table_name))
}

fn known_event_columns(table_name: &str) -> Option<&'static [&'static str]> {
    match table_name {
        "data_migration" => Some(&["name", "time_completed"]),
        "migration" => Some(&["id", "time_completed"]),
        "event" => Some(&["id", "aggregate_id", "seq", "type", "data"]),
        "event_sequence" => Some(&["aggregate_id", "seq", "owner_id"]),
        _ => None,
    }
}

fn known_session_columns(table_name: &str) -> Option<&'static [&'static str]> {
    match table_name {
        "message" => Some(&["id", "session_id", "time_created", "time_updated", "data"]),
        "part" => Some(&[
            "id",
            "message_id",
            "session_id",
            "time_created",
            "time_updated",
            "data",
        ]),
        "session" => Some(&[
            "id",
            "project_id",
            "workspace_id",
            "parent_id",
            "slug",
            "directory",
            "path",
            "title",
            "version",
            "share_url",
            "summary_additions",
            "summary_deletions",
            "summary_files",
            "summary_diffs",
            "metadata",
            "cost",
            "tokens_input",
            "tokens_output",
            "tokens_reasoning",
            "tokens_cache_read",
            "tokens_cache_write",
            "revert",
            "permission",
            "agent",
            "model",
            "time_created",
            "time_updated",
            "time_compacting",
            "time_archived",
        ]),
        "session_context_epoch" => Some(&["session_id", "baseline", "snapshot", "baseline_seq"]),
        "session_input" => Some(&[
            "id",
            "session_id",
            "prompt",
            "delivery",
            "admitted_seq",
            "promoted_seq",
            "time_created",
        ]),
        "session_message" => Some(&[
            "id",
            "session_id",
            "type",
            "seq",
            "time_created",
            "time_updated",
            "data",
        ]),
        "session_share" => Some(&[
            "session_id",
            "id",
            "secret",
            "url",
            "time_created",
            "time_updated",
        ]),
        "todo" => Some(&[
            "session_id",
            "content",
            "status",
            "priority",
            "position",
            "time_created",
            "time_updated",
        ]),
        _ => None,
    }
}

fn known_control_columns(table_name: &str) -> Option<&'static [&'static str]> {
    match table_name {
        "account" => Some(&[
            "id",
            "email",
            "url",
            "access_token",
            "refresh_token",
            "token_expiry",
            "time_created",
            "time_updated",
        ]),
        "account_state" => Some(&["id", "active_account_id", "active_org_id"]),
        "control_account" => Some(&[
            "email",
            "url",
            "access_token",
            "refresh_token",
            "token_expiry",
            "active",
            "time_created",
            "time_updated",
        ]),
        "credential" => Some(&[
            "id",
            "integration_id",
            "label",
            "value",
            "connector_id",
            "method_id",
            "active",
            "time_created",
            "time_updated",
        ]),
        "permission" => Some(&[
            "id",
            "project_id",
            "action",
            "resource",
            "time_created",
            "time_updated",
        ]),
        "project" => Some(&[
            "id",
            "worktree",
            "vcs",
            "name",
            "icon_url",
            "icon_url_override",
            "icon_color",
            "time_created",
            "time_updated",
            "time_initialized",
            "sandboxes",
            "commands",
        ]),
        "project_directory" => Some(&[
            "project_id",
            "directory",
            "type",
            "strategy",
            "time_created",
        ]),
        "workspace" => Some(&[
            "id",
            "type",
            "name",
            "branch",
            "directory",
            "extra",
            "project_id",
            "time_used",
        ]),
        _ => None,
    }
}

const KNOWN_INDEXES: &[&str] = &[
    "event_aggregate_seq_idx",
    "event_aggregate_type_seq_idx",
    "message_session_time_created_id_idx",
    "part_message_id_id_idx",
    "part_session_idx",
    "permission_project_action_resource_idx",
    "session_input_session_admitted_seq_idx",
    "session_input_session_pending_delivery_seq_idx",
    "session_input_session_promoted_seq_idx",
    "session_message_session_seq_idx",
    "session_message_session_time_created_id_idx",
    "session_message_session_type_seq_idx",
    "session_message_time_created_idx",
    "session_parent_idx",
    "session_project_idx",
    "session_workspace_idx",
    "todo_session_idx",
];

const DELETE_TARGETS: &[&str] = &["event_sequence", "project", "session"];

#[derive(Clone, Debug, Eq, PartialEq)]
struct ForeignKey {
    source_table: String,
    source_column: String,
    target_table: String,
    target_column: String,
    on_delete: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct KnownForeignKey {
    source_table: &'static str,
    source_column: &'static str,
    target_table: &'static str,
    target_column: &'static str,
    on_delete: &'static str,
}

fn is_known_foreign_key(actual: &ForeignKey) -> bool {
    KNOWN_FOREIGN_KEYS.iter().any(|known| {
        actual.source_table == known.source_table
            && actual.source_column == known.source_column
            && actual.target_table == known.target_table
            && actual.target_column == known.target_column
            && actual.on_delete.eq_ignore_ascii_case(known.on_delete)
    })
}

const KNOWN_FOREIGN_KEYS: &[KnownForeignKey] = &[
    KnownForeignKey {
        source_table: "account_state",
        source_column: "active_account_id",
        target_table: "account",
        target_column: "id",
        on_delete: "SET NULL",
    },
    KnownForeignKey {
        source_table: "event",
        source_column: "aggregate_id",
        target_table: "event_sequence",
        target_column: "aggregate_id",
        on_delete: "CASCADE",
    },
    KnownForeignKey {
        source_table: "message",
        source_column: "session_id",
        target_table: "session",
        target_column: "id",
        on_delete: "CASCADE",
    },
    KnownForeignKey {
        source_table: "part",
        source_column: "message_id",
        target_table: "message",
        target_column: "id",
        on_delete: "CASCADE",
    },
    KnownForeignKey {
        source_table: "permission",
        source_column: "project_id",
        target_table: "project",
        target_column: "id",
        on_delete: "CASCADE",
    },
    KnownForeignKey {
        source_table: "project_directory",
        source_column: "project_id",
        target_table: "project",
        target_column: "id",
        on_delete: "CASCADE",
    },
    KnownForeignKey {
        source_table: "session",
        source_column: "project_id",
        target_table: "project",
        target_column: "id",
        on_delete: "CASCADE",
    },
    KnownForeignKey {
        source_table: "session_context_epoch",
        source_column: "session_id",
        target_table: "session",
        target_column: "id",
        on_delete: "CASCADE",
    },
    KnownForeignKey {
        source_table: "session_input",
        source_column: "session_id",
        target_table: "session",
        target_column: "id",
        on_delete: "CASCADE",
    },
    KnownForeignKey {
        source_table: "session_message",
        source_column: "session_id",
        target_table: "session",
        target_column: "id",
        on_delete: "CASCADE",
    },
    KnownForeignKey {
        source_table: "session_share",
        source_column: "session_id",
        target_table: "session",
        target_column: "id",
        on_delete: "CASCADE",
    },
    KnownForeignKey {
        source_table: "todo",
        source_column: "session_id",
        target_table: "session",
        target_column: "id",
        on_delete: "CASCADE",
    },
    KnownForeignKey {
        source_table: "workspace",
        source_column: "project_id",
        target_table: "project",
        target_column: "id",
        on_delete: "CASCADE",
    },
];
