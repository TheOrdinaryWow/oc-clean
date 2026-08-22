use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::params;

#[cfg(feature = "bench-large")]
use oc_clean::delete::sessions::DeleteOptions;
#[cfg(feature = "bench-large")]
use std::fs::OpenOptions;
#[cfg(feature = "bench-large")]
use std::process::{Command, Output};
#[cfg(feature = "bench-large")]
use std::time::{Duration, Instant};

#[path = "support/fixture.rs"]
#[allow(dead_code)]
mod fixture;

use fixture::{Fixture, FixtureConfig};

const LARGE_TABLES: &[&str] = &["session", "message", "part"];

#[test]
fn sql_literals_do_not_apply_length_to_payload_columns() {
    let payload_columns = ["data", "baseline", "snapshot"];
    let violations = rust_sources(Path::new("src"))
        .into_iter()
        .flat_map(|path| {
            let source = fs::read_to_string(&path).expect("Rust source should be readable");
            sql_literals(&source)
                .into_iter()
                .filter_map(|sql| {
                    length_arguments(&sql)
                        .into_iter()
                        .find(|argument| {
                            payload_columns
                                .iter()
                                .any(|column| sql_identifier_occurs(argument, column))
                        })
                        .map(|argument| format!("{}: LENGTH({argument})", path.display()))
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    assert_eq!(
        violations,
        Vec::<String>::new(),
        "payload sizing must use octet_length(): {violations:?}"
    );
}

#[test]
fn analysis_sql_uses_plain_integer_epoch_comparisons() {
    let forbidden = ["julianday(", "datetime("];
    let violations = rust_sources(Path::new("src/analyze"))
        .into_iter()
        .flat_map(|path| {
            let source = fs::read_to_string(&path).expect("analysis source should be readable");
            let display_path = path.display().to_string();
            sql_literals(&source)
                .into_iter()
                .flat_map(|sql| {
                    let normalized = sql.to_ascii_lowercase();
                    forbidden
                        .iter()
                        .filter(move |function| normalized.contains(**function))
                        .map(|function| format!("{display_path}: {function}"))
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    assert_eq!(
        violations,
        Vec::<String>::new(),
        "analysis timestamps must remain index-friendly integer comparisons"
    );
}

#[test]
fn row_iteration_loops_do_not_issue_follow_up_sql() {
    let violations = rust_sources(Path::new("src"))
        .into_iter()
        .flat_map(|path| {
            let source = fs::read_to_string(&path).expect("Rust source should be readable");
            sql_calls_in_row_loops(&source)
                .into_iter()
                .map(|call| format!("{}: {call}", path.display()))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    assert_eq!(
        violations,
        Vec::<String>::new(),
        "row-driven SQL calls create N+1 query behavior"
    );
}

#[test]
fn required_selection_indexes_are_used_without_large_table_scans() {
    let fixture = Fixture::build(&FixtureConfig {
        project_count: 2,
        session_count: 8,
        messages_per_session: 2,
        parts_per_message: 2,
        sub_session_depth: 1,
        sub_session_fan_out: 1,
        ..FixtureConfig::default()
    })
    .expect("query-plan fixture should build");
    let connection = fixture.connect().expect("fixture should connect");
    let project_id = connection
        .query_row("SELECT project_id FROM session LIMIT 1", [], |row| {
            row.get::<_, String>(0)
        })
        .expect("fixture should contain a project session");
    let session_id = connection
        .query_row("SELECT id FROM session LIMIT 1", [], |row| {
            row.get::<_, String>(0)
        })
        .expect("fixture should contain a session");

    let cases = [
        (
            "SELECT id FROM session WHERE project_id = ?1",
            project_id.as_str(),
            "session_project_idx",
        ),
        (
            "SELECT id FROM session WHERE parent_id = ?1",
            session_id.as_str(),
            "session_parent_idx",
        ),
        (
            "SELECT id FROM part WHERE session_id = ?1",
            session_id.as_str(),
            "part_session_idx",
        ),
        (
            "SELECT id FROM message WHERE session_id = ?1 ORDER BY time_created, id",
            session_id.as_str(),
            "message_session_time_created_id_idx",
        ),
    ];

    for (sql, parameter, expected_index) in cases {
        let details = explain_query_plan(&connection, sql, parameter);
        assert!(
            details.iter().any(|detail| detail.contains(expected_index)),
            "expected {expected_index} for `{sql}`, got {details:?}"
        );
        let unexpected_scans = details
            .iter()
            .filter(|detail| {
                let normalized = detail.to_ascii_lowercase();
                LARGE_TABLES
                    .iter()
                    .any(|table| normalized.contains(&format!("scan {table}")))
            })
            .collect::<Vec<_>>();
        assert!(
            unexpected_scans.is_empty(),
            "hot query `{sql}` scans a large table: {unexpected_scans:?}"
        );
    }
}

#[test]
fn regression_reader_cannot_modify_the_committed_baseline() {
    let perf_source = include_str!("perf.rs");
    let generator_name = ["gen", "baseline"].join("-");
    assert!(
        !perf_source.contains(&format!("Command::new({generator_name:?})")),
        "the regression suite must never invoke the baseline generator"
    );

    #[cfg(feature = "bench-large")]
    {
        let path = baseline_path();
        let before = fs::read(&path).expect("committed benchmark baseline should be readable");
        let _baseline = read_baseline(&path);
        let after = fs::read(&path).expect("committed benchmark baseline should remain readable");
        assert_eq!(
            after, before,
            "the regression reader changed benchmarks.json"
        );
    }
}

#[cfg(feature = "bench-large")]
#[test]
#[ignore = "generates and analyzes a multi-gigabyte fixture"]
fn performance_budgets_hold_on_the_committed_large_fixture_scale() {
    let baseline = read_baseline(&baseline_path());
    let fixture = Fixture::build(&FixtureConfig::bench_large(baseline.target_size_bytes))
        .expect("large performance fixture should build");
    let report = fixture
        .large_report()
        .expect("bench-large fixture should carry a generation report");

    let (cold, _) = timed_analyze(&fixture.database_path, true);
    let (warm, _) = timed_analyze(&fixture.database_path, true);
    let (standard, standard_output) = timed_analyze(&fixture.database_path, false);
    let standard_json: serde_json::Value = serde_json::from_slice(&standard_output.stdout)
        .expect("standard analysis should emit one JSON report");
    assert_eq!(standard_json["mode"], "standard");

    println!(
        "fixture_bytes={} sessions={} messages={} parts={} generation_s={:.3}",
        report.achieved_size_bytes,
        report.session_count,
        report.message_count,
        report.part_count,
        report.generation_time.as_secs_f64()
    );
    println!(
        "analyze_detailed_cold_s={:.3} analyze_detailed_warm_s={:.3} analyze_standard_s={:.3}",
        cold.as_secs_f64(),
        warm.as_secs_f64(),
        standard.as_secs_f64()
    );

    // A standard report skips the object-space walk and the external-directory stat sweep, so it
    // must stay meaningfully cheaper than the detailed one it is carved out of.
    assert!(
        standard < cold,
        "analyze took {:.3}s standard versus {:.3}s detailed; the standard path must skip work",
        standard.as_secs_f64(),
        cold.as_secs_f64()
    );
    assert_relative_budget("cold", cold, baseline.detailed_cold_ms);
    assert_relative_budget("warm", warm, baseline.detailed_warm_ms);
    assert_eq!(
        DeleteOptions::default().batch_size,
        baseline.fastest_delete_batch_size,
        "default delete batch size should match the fastest committed measurement"
    );
}

fn explain_query_plan(
    connection: &rusqlite::Connection,
    sql: &str,
    parameter: &str,
) -> Vec<String> {
    let mut statement = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("query plan should prepare");
    statement
        .query_map(params![parameter], |row| row.get::<_, String>(3))
        .expect("query plan should execute")
        .collect::<Result<Vec<_>, _>>()
        .expect("query plan rows should decode")
}

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    fn visit(directory: &Path, files: &mut Vec<PathBuf>) {
        let entries = fs::read_dir(directory).expect("source directory should be readable");
        for entry in entries {
            let path = entry.expect("source entry should be readable").path();
            if path.is_dir() {
                visit(&path, files);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }

    let mut files = Vec::new();
    visit(root, &mut files);
    files.sort();
    files
}

fn sql_literals(source: &str) -> Vec<String> {
    rust_string_literals(source)
        .into_iter()
        .filter(|literal| {
            let trimmed = literal.trim_start().to_ascii_lowercase();
            [
                "select ", "with ", "delete ", "insert ", "update ", "create ", "drop ", "pragma ",
                "explain ", "vacuum ", "begin ", "commit", "rollback",
            ]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix))
        })
        .collect()
}

fn rust_string_literals(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut literals = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'r' {
            let mut marker = index + 1;
            while marker < bytes.len() && bytes[marker] == b'#' {
                marker += 1;
            }
            if marker < bytes.len() && bytes[marker] == b'"' {
                let hashes = marker - index - 1;
                let content_start = marker + 1;
                let mut end = content_start;
                while end < bytes.len() {
                    if bytes[end] == b'"'
                        && end + hashes < bytes.len()
                        && (hashes == 0
                            || bytes[end + 1..=end + hashes]
                                .iter()
                                .all(|byte| *byte == b'#'))
                    {
                        literals.push(source[content_start..end].to_owned());
                        index = end + hashes + 1;
                        break;
                    }
                    end += 1;
                }
                if end < bytes.len() {
                    continue;
                }
            }
        }
        if bytes[index] == b'"' {
            let content_start = index + 1;
            let mut end = content_start;
            while end < bytes.len() {
                match bytes[end] {
                    b'\\' => end = end.saturating_add(2),
                    b'"' => {
                        literals.push(source[content_start..end].to_owned());
                        index = end + 1;
                        break;
                    }
                    _ => end += 1,
                }
            }
            if end < bytes.len() {
                continue;
            }
        }
        index += 1;
    }
    literals
}

fn length_arguments(sql: &str) -> Vec<&str> {
    let normalized = sql.to_ascii_lowercase();
    let bytes = normalized.as_bytes();
    let mut arguments = Vec::new();
    let mut offset = 0;
    while let Some(found) = normalized[offset..].find("length(") {
        let start = offset + found;
        let is_identifier_suffix = start.checked_sub(1).is_some_and(|previous| {
            bytes[previous].is_ascii_alphanumeric() || bytes[previous] == b'_'
        });
        let argument_start = start + "length(".len();
        if !is_identifier_suffix && let Some(argument_end) = normalized[argument_start..].find(')')
        {
            arguments.push(&sql[argument_start..argument_start + argument_end]);
        }
        offset = argument_start;
    }
    arguments
}

fn sql_identifier_occurs(expression: &str, identifier: &str) -> bool {
    expression
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .any(|token| token.eq_ignore_ascii_case(identifier))
}

fn sql_calls_in_row_loops(source: &str) -> Vec<String> {
    let loop_markers = ["while let Some(row)", "while let Some(row_result)"];
    let sql_calls = [".prepare(", ".query(", ".query_row(", ".execute("];
    let mut violations = Vec::new();
    for marker in loop_markers {
        let mut offset = 0;
        while let Some(found) = source[offset..].find(marker) {
            let loop_start = offset + found;
            let Some(open_brace) = source[loop_start..]
                .find('{')
                .map(|relative| loop_start + relative)
            else {
                break;
            };
            let Some(close_brace) = matching_brace(source, open_brace) else {
                break;
            };
            let body = &source[open_brace + 1..close_brace];
            violations.extend(
                sql_calls
                    .iter()
                    .filter(|call| body.contains(**call))
                    .map(|call| (*call).to_owned()),
            );
            offset = close_brace + 1;
        }
    }
    violations
}

fn matching_brace(source: &str, open_brace: usize) -> Option<usize> {
    let mut depth = 0_usize;
    for (relative, byte) in source.as_bytes()[open_brace..].iter().enumerate() {
        match byte {
            b'{' => depth = depth.saturating_add(1),
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open_brace + relative);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(feature = "bench-large")]
struct Baseline {
    target_size_bytes: u64,
    detailed_cold_ms: f64,
    detailed_warm_ms: f64,
    fastest_delete_batch_size: usize,
}

#[cfg(feature = "bench-large")]
fn baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("benchmarks.json")
}

#[cfg(feature = "bench-large")]
fn read_baseline(path: &Path) -> Baseline {
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .expect("committed benchmark baseline should open read-only");
    let value: serde_json::Value =
        serde_json::from_reader(file).expect("benchmark baseline should be valid JSON");
    let delete_measurements = value["delete_batch_tuning"]
        .as_array()
        .expect("delete batch tuning should be an array");
    let fastest_delete_batch_size = delete_measurements
        .iter()
        .min_by(|left, right| {
            left["elapsed_ms"]
                .as_f64()
                .expect("delete elapsed time should be numeric")
                .total_cmp(
                    &right["elapsed_ms"]
                        .as_f64()
                        .expect("delete elapsed time should be numeric"),
                )
        })
        .and_then(|measurement| measurement["batch_size"].as_u64())
        .and_then(|batch_size| usize::try_from(batch_size).ok())
        .expect("delete tuning should contain a representable fastest batch size");
    Baseline {
        target_size_bytes: value["fixture"]["target_size_bytes"]
            .as_u64()
            .expect("fixture target should be an integer"),
        detailed_cold_ms: value["analyze"]["detailed_cold_ms"]
            .as_f64()
            .expect("cold baseline should be numeric"),
        detailed_warm_ms: value["analyze"]["detailed_warm_ms"]
            .as_f64()
            .expect("warm baseline should be numeric"),
        fastest_delete_batch_size,
    }
}

#[cfg(feature = "bench-large")]
fn timed_analyze(database_path: &Path, detailed: bool) -> (Duration, Output) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oc-clean"));
    command
        .arg("--db")
        .arg(database_path)
        .arg("analyze")
        .arg("--json");
    if detailed {
        command.arg("--detailed");
    }
    let started = Instant::now();
    let output = command
        .output()
        .expect("compiled oc-clean binary should run");
    let elapsed = started.elapsed();
    assert!(
        output.status.success(),
        "analysis failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (elapsed, output)
}

#[cfg(feature = "bench-large")]
fn assert_relative_budget(cache_state: &str, measured: Duration, baseline_ms: f64) {
    let limit_ms = baseline_ms * 1.2;
    let measured_ms = measured.as_secs_f64() * 1_000.0;
    assert!(
        measured_ms <= limit_ms,
        "full {cache_state} analyze took {measured_ms:.3}ms; 1.2x baseline is {limit_ms:.3}ms"
    );
}
