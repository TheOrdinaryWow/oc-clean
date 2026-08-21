use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oc_clean::cli::{AnalyzeArgs, Cli, Commands, LogFormat};
use oc_clean::db::{self, ConnectionOptions};
use oc_clean::delete::sessions::{self, DeleteOptions};
use oc_clean::paths::Target;
use oc_clean::report;

#[path = "fixture.rs"]
#[rustfmt::skip]
#[allow(dead_code)]
mod fixture;

use fixture::{Fixture, FixtureConfig, LargeFixtureReport};

const TARGET_SIZE_BYTES: u64 = 2_000_000_000;
const DELETE_BATCH_SIZES: &[usize] = &[1_000, 2_500, 5_000, 10_000];
type BaselineResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

fn main() -> BaselineResult<()> {
    let output_path = output_path();
    let fixture = Fixture::build(&FixtureConfig::bench_large(TARGET_SIZE_BYTES))?;
    let report = fixture
        .large_report()
        .expect("bench-large fixture should carry a generation report");
    let cold = measure_report(&fixture.database_path, false)?;
    let warm = measure_report(&fixture.database_path, false)?;
    let quick = measure_report(&fixture.database_path, true)?;
    let delete_measurements = measure_delete_batches()?;

    fs::write(
        &output_path,
        serde_json::to_vec_pretty(&baseline_document(
            report,
            cold,
            warm,
            quick,
            &delete_measurements,
        ))?,
    )?;

    println!("baseline={}", output_path.display());
    println!(
        "fixture_bytes={} sessions={} messages={} parts={} generation_s={:.3}",
        report.achieved_size_bytes,
        report.session_count,
        report.message_count,
        report.part_count,
        report.generation_time.as_secs_f64()
    );
    println!(
        "analyze_full_cold_s={:.3} analyze_full_warm_s={:.3} analyze_quick_s={:.3}",
        cold.as_secs_f64(),
        warm.as_secs_f64(),
        quick.as_secs_f64()
    );
    for measurement in &delete_measurements {
        println!(
            "delete_batch_size={} elapsed_s={:.3} transactions={} sessions={}",
            measurement.batch_size,
            measurement.elapsed.as_secs_f64(),
            measurement.transactions,
            measurement.session_count
        );
    }
    Ok(())
}

fn output_path() -> PathBuf {
    env::args_os().nth(1).map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("benchmarks.json"),
        PathBuf::from,
    )
}

fn measure_report(database_path: &Path, quick: bool) -> BaselineResult<Duration> {
    let cli = Cli {
        command: Commands::Analyze(AnalyzeArgs {
            json: true,
            log_format: LogFormat::Text,
            top: 10,
            quick,
        }),
        db: Some(database_path.to_owned()),
        apply: false,
        force: false,
        force_schema: false,
        dangerously_skip_confirm: false,
        skip_backup: false,
    };
    let Commands::Analyze(arguments) = &cli.command else {
        unreachable!("constructed command should be analyze");
    };
    let mut output = io::sink();
    let started = Instant::now();
    report::command::run(&cli, arguments, &mut output)?;
    Ok(started.elapsed())
}

struct DeleteMeasurement {
    batch_size: usize,
    elapsed: Duration,
    transactions: u64,
    session_count: usize,
}

fn measure_delete_batches() -> BaselineResult<Vec<DeleteMeasurement>> {
    DELETE_BATCH_SIZES
        .iter()
        .copied()
        .map(measure_delete_batch)
        .collect()
}

fn measure_delete_batch(batch_size: usize) -> BaselineResult<DeleteMeasurement> {
    let fixture = Fixture::build(&FixtureConfig::bench_large(TARGET_SIZE_BYTES))?;
    let database = db::open_read_write(
        &Target::File(fixture.database_path.clone()),
        ConnectionOptions::default(),
    )?;
    let session_ids = database
        .connection()
        .prepare("SELECT id FROM session ORDER BY id")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<BTreeSet<_>, _>>()?;
    let session_count = session_ids.len();
    let started = Instant::now();
    let deletion = sessions::delete(
        &database,
        &session_ids,
        DeleteOptions {
            batch_size,
            batch_time_limit: Duration::from_secs(30),
        },
    )?;
    Ok(DeleteMeasurement {
        batch_size,
        elapsed: started.elapsed(),
        transactions: deletion.transactions,
        session_count,
    })
}

fn baseline_document(
    report: &LargeFixtureReport,
    cold: Duration,
    warm: Duration,
    quick: Duration,
    delete_measurements: &[DeleteMeasurement],
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "fixture": {
            "target_size_bytes": report.target_size_bytes,
            "achieved_size_bytes": report.achieved_size_bytes,
            "project_count": report.project_count,
            "session_count": report.session_count,
            "message_count": report.message_count,
            "part_count": report.part_count,
            "generation_ms": milliseconds(report.generation_time),
        },
        "analyze": {
            "full_cold_ms": milliseconds(cold),
            "full_warm_ms": milliseconds(warm),
            "quick_ms": milliseconds(quick),
        },
        "delete_batch_tuning": delete_measurements
            .iter()
            .map(|measurement| serde_json::json!({
                "batch_size": measurement.batch_size,
                "elapsed_ms": milliseconds(measurement.elapsed),
                "transactions": measurement.transactions,
                "session_count": measurement.session_count,
            }))
            .collect::<Vec<_>>(),
    })
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
