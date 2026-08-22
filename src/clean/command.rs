use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::cli::{CleanArgs, Cli};
use crate::db::{self, ConnectionOptions};
use crate::delete::orphans::{self, OrphanDeleteOptions};
use crate::delete::projects::{self, ProjectIds};
use crate::delete::sessions::{self, DeleteOptions, DeletionReport};
use crate::doctor::foreign_key_check;
use crate::error::Error;
use crate::parallel::{self, JobHandle};
use crate::paths::{self, DatabaseOptions, DerivedPaths, Environment, Platform, Target};
use crate::reclaim::headroom::{FreeSpaceProvider, Fs4FreeSpaceProvider};
use crate::reclaim::incremental::{IncrementalVacuumError, check_auto_vacuum};
use crate::report::format::{self, Style};
use crate::report::impact::{self, Impact, ReportMode};
use crate::report::progress;
use crate::safety::confirm::{
    CONFIRMATION_ATTEMPTS, ConfirmationDecision, ConfirmationOptions, ImpactSummary, RefusalReason,
    confirm, warrants_escalation,
};
use crate::safety::holders::{CommandMode, GateDecision, HolderInspector, inspect_and_decide};
use crate::select::orphans::RawOrphans;
use crate::select::predicates::SessionIds;

use super::cleanup::CleanupOutcome;
use super::output::{self, CleanReport};
use super::progress::ProgressPhaseObserver;
use super::selection;
use super::signal::SignalController;
use super::{PhaseId, PhaseObserver, PhaseOperation, reclaim};

#[derive(Clone, Copy)]
pub(super) struct RuntimeContext {
    pub(super) stdin_is_terminal: bool,
    pub(super) stdout_is_terminal: bool,
}

/// Executes a clean dry-run or mutation through the complete safety pipeline.
///
/// # Errors
///
/// Returns the typed failure assigned to the phase that stopped the pipeline.
pub fn run(cli: &Cli, arguments: &CleanArgs, output: &mut dyn Write) -> Result<(), Error> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let signals = SignalController::new();
    signals.install()?;
    let observer = ProgressPhaseObserver::new(arguments, !cli.dry_run);
    let result = run_with(
        cli,
        arguments,
        &mut input,
        output,
        &Fs4FreeSpaceProvider,
        platform_inspector().as_ref(),
        RuntimeContext {
            stdin_is_terminal: io::stdin().is_terminal(),
            stdout_is_terminal: io::stdout().is_terminal(),
        },
        &signals,
        &observer,
    );
    observer.finish();
    result
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn run_with<R, P>(
    cli: &Cli,
    arguments: &CleanArgs,
    input: &mut R,
    output: &mut dyn Write,
    free_space: &P,
    holder_inspector: &(dyn HolderInspector + Sync),
    runtime: RuntimeContext,
    signals: &SignalController,
    observer: &dyn PhaseObserver,
) -> Result<(), Error>
where
    R: BufRead + ?Sized,
    P: FreeSpaceProvider,
{
    run_with_git_path(
        cli,
        arguments,
        input,
        output,
        free_space,
        holder_inspector,
        runtime,
        signals,
        observer,
        None,
    )
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn run_with_git_path<R, P>(
    cli: &Cli,
    arguments: &CleanArgs,
    input: &mut R,
    output: &mut dyn Write,
    free_space: &P,
    holder_inspector: &(dyn HolderInspector + Sync),
    runtime: RuntimeContext,
    signals: &SignalController,
    observer: &dyn PhaseObserver,
    git_path: Option<&OsStr>,
) -> Result<(), Error>
where
    R: BufRead + ?Sized,
    P: FreeSpaceProvider,
{
    phase(observer, PhaseId::P1);
    let target = database_target(cli)?;
    let database_path = file_path(&target)?;
    let paths = derived_paths(database_path)?;
    phase(observer, PhaseId::P2);
    phase(observer, PhaseId::P3);
    if arguments.incremental {
        phase(observer, PhaseId::P3b);
    }
    phase(observer, PhaseId::P4);
    let (pre_delete_space, hard_links) =
        inspect_read_only_phases(&target, database_path, cli, arguments, holder_inspector)?;
    stop_before_mutation_if_cancelled(signals)?;

    let database = db::open_read_write(&target, ConnectionOptions::default())?;
    signals.set_interrupt_handle(database.interrupt_handle());
    let locked_data_version = if cli.dry_run {
        None
    } else {
        phase(observer, PhaseId::P5);
        database.acquire_exclusive_lock()?;
        Some(database.data_version()?)
    };

    let now_ms = selection::now_ms()?;
    phase(observer, PhaseId::P6);
    observer.performing(PhaseId::P6, PhaseOperation::Retention);
    let retained = selection::retention_set(&database, arguments.keep_recent)?;
    phase(observer, PhaseId::P7);
    observer.performing(PhaseId::P7, PhaseOperation::PredicateSelection);
    let candidates = selection::predicate_candidates(&database, arguments, &retained, now_ms)?;
    phase(observer, PhaseId::P8);
    observer.performing(PhaseId::P8, PhaseOperation::DescendantExpansion);
    let selected = selection::expand_candidates(&database, &candidates, &retained)?;

    phase(observer, PhaseId::P9);
    let impact = impact::summarize(
        &database,
        &paths,
        &selection::impact_selection(arguments, now_ms),
    )?;
    if !arguments.incremental && !arguments.no_vacuum {
        let mut deletion_batch_ids = impact.session_ids.clone();
        deletion_batch_ids.extend(impact.orphan_event_aggregate_ids.iter().cloned());
        reclaim::pre_delete_headroom(
            database_path,
            &pre_delete_space,
            impact.summary.current_live_bytes,
            impact.summary.database_bytes,
            deletion_batch_ids.len(),
            hard_links || cli.skip_backup,
            free_space,
        )?;
    }
    phase(observer, PhaseId::P10);
    if cli.dry_run {
        return output::write_dry_run(&impact.summary, arguments.json, report_style(), output)
            .map_err(output_error);
    }
    // Entering P11 clears the phase bar. It must happen before the report and the prompt are
    // written, because the bar redraws on stderr and would interleave with them otherwise.
    phase(observer, PhaseId::P11);
    render_apply_impact(arguments, &impact, output)?;
    ensure_confirmed(cli, arguments, input, output, runtime, &impact)?;
    stop_before_mutation_if_cancelled(signals)?;

    let mut affected_projects = projects::owners_of_sessions(&database, &selected)?;
    signals.begin_delete();
    phase(observer, PhaseId::P12);
    let delete_bar = progress::counted("delete", selected.len() as u64);
    delete_bar.set_message("deleting sessions");
    let session_report =
        sessions::delete_with_progress(&database, &selected, DeleteOptions::default(), |report| {
            delete_bar.set_position(report.deleted_session_ids.len() as u64);
            batch_committed(report, signals)
        });
    delete_bar.finish();
    let session_report = session_report?;
    stop_after_delete_if_cancelled(&session_report, arguments, output, signals)?;

    let mut orphan_report = None;
    if arguments.orphans {
        phase(observer, PhaseId::P13);
        observer.performing(PhaseId::P13, PhaseOperation::OrphanSelection);
        let raw_orphans = crate::select::orphans::select(&database, &paths)?;
        affected_projects.extend(orphan_project_owners(&database, &raw_orphans)?);
        let orphan_bar = progress::spinner("orphans", "deleting orphaned rows");
        let report = orphans::delete_with_progress(
            &database,
            &raw_orphans,
            OrphanDeleteOptions {
                keep_recent: arguments.keep_recent,
                ..OrphanDeleteOptions::default()
            },
            |report| {
                orphan_bar.set_message(format!("committed {} transactions", report.transactions));
                batch_committed(report, signals)
            },
        );
        orphan_bar.finish();
        let report = report?;
        let combined = combine_reports(&session_report, &report.deletion);
        stop_after_delete_if_cancelled(&combined, arguments, output, signals)?;
        orphan_report = Some(report.deletion);
    }

    phase(observer, PhaseId::P14);
    let pruned_projects = projects::prune(
        &database,
        &affected_projects,
        arguments.prune_empty_projects,
    )?;
    let combined = combine_reports(
        &session_report,
        orphan_report.as_ref().unwrap_or(&DeletionReport::default()),
    );
    stop_after_delete_if_cancelled(&combined, arguments, output, signals)?;

    phase(observer, PhaseId::P15);
    let integrity = crate::doctor::integrity_check(database.connection())?;
    if !integrity.ok {
        return Err(Error::IntegrityCheckFailed {
            check: "integrity_check".to_owned(),
            message: integrity.findings.join("; "),
        });
    }
    let foreign_keys = foreign_key_check(database.connection())?;
    if !foreign_keys.ok {
        return Err(Error::IntegrityCheckFailed {
            check: "foreign_key_check".to_owned(),
            message: format!("{:?}", foreign_keys.findings),
        });
    }
    stop_after_delete_if_cancelled(&combined, arguments, output, signals)?;

    let deleted_session_ids = combined.deleted_session_ids.clone();
    let mut cleanup = CleanupOutcome::default();
    phase(observer, PhaseId::P16);
    cleanup.sweep_this_run(&database, &paths, &deleted_session_ids);
    stop_after_delete_if_cancelled(&combined, arguments, output, signals)?;
    phase(observer, PhaseId::P17);
    cleanup.remove_pruned(&paths, &pruned_projects);
    stop_after_delete_if_cancelled(&combined, arguments, output, signals)?;
    if arguments.orphans {
        phase(observer, PhaseId::P17b);
        cleanup.sweep_preexisting_orphans(&database, &paths);
        stop_after_delete_if_cancelled(&combined, arguments, output, signals)?;
    }
    if arguments.gc_snapshots {
        phase(observer, PhaseId::P18);
        cleanup.gc_retained(&database, &paths, &pruned_projects, git_path);
        stop_after_delete_if_cancelled(&combined, arguments, output, signals)?;
    }

    if !arguments.incremental && !arguments.no_vacuum {
        phase(observer, PhaseId::P19);
        reclaim::post_delete_headroom(&database, database_path, cli, arguments, free_space)
            .inspect_err(|_error| {
                warn!("rows deleted, space not reclaimed - free space and run `oc-clean vacuum`");
            })?;
        stop_after_delete_if_cancelled(&combined, arguments, output, signals)?;
    }

    let bytes_reclaimed = if arguments.no_vacuum {
        drop(database);
        0
    } else {
        signals.begin_reclaim();
        phase(observer, PhaseId::P20);
        let data_version = locked_data_version.ok_or_else(|| Error::DatabaseBusy {
            holders: vec!["exclusive lock was not acquired".to_owned()],
        })?;
        match reclaim::run(
            database,
            database_path,
            data_version,
            cli,
            arguments,
            signals,
        ) {
            Err(error) if error.exit_code() == 8 => {
                output::write_interrupted(
                    u64::try_from(deleted_session_ids.len()).unwrap_or(u64::MAX),
                    arguments.json,
                    output,
                )
                .map_err(output_error)?;
                return Err(error);
            }
            result => result?,
        }
    };

    phase(observer, PhaseId::P21);
    let report = final_report(
        impact,
        &combined,
        &pruned_projects,
        cleanup,
        bytes_reclaimed,
    );
    output::write_final(&report, arguments.json, report_style(), output).map_err(output_error)?;
    if report.partial_failures.is_empty() {
        Ok(())
    } else {
        Err(Error::PartialSuccess {
            left_behind: report.partial_failures.join(", "),
        })
    }
}

fn batch_committed(report: &DeletionReport, signals: &SignalController) -> bool {
    info!(
        transactions = report.transactions,
        deleted_sessions = report.deleted_session_ids.len(),
        "committed delete batch"
    );
    !signals.cancelled()
}

fn combine_reports(left: &DeletionReport, right: &DeletionReport) -> DeletionReport {
    let mut report = left.clone();
    for (table, rows) in &right.table_rows {
        *report.table_rows.entry(table.clone()).or_default() += rows;
    }
    report.transactions = report.transactions.saturating_add(right.transactions);
    report
        .deleted_session_ids
        .extend(right.deleted_session_ids.iter().cloned());
    report
}

fn orphan_project_owners(
    database: &db::ReadWriteConnection,
    raw: &RawOrphans,
) -> Result<ProjectIds, Error> {
    let session_ids = raw
        .dangling_session_ids
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect::<SessionIds>();
    projects::owners_of_sessions(database, &session_ids)
}

fn final_report(
    impact: Impact,
    deletion: &DeletionReport,
    pruned: &BTreeSet<String>,
    cleanup: CleanupOutcome,
    bytes_reclaimed: u64,
) -> CleanReport {
    CleanReport {
        impact: impact.summary,
        applied: true,
        deleted_sessions: u64::try_from(deletion.deleted_session_ids.len()).unwrap_or(u64::MAX),
        deleted_rows: deletion.table_rows.values().copied().sum(),
        pruned_projects: u64::try_from(pruned.len()).unwrap_or(u64::MAX),
        storage_files_removed: cleanup.storage_files_removed,
        snapshot_directories_removed: cleanup.snapshot_directories_removed,
        bytes_reclaimed,
        partial_failures: cleanup.partial_failures,
    }
}

fn stop_after_delete_if_cancelled(
    report: &DeletionReport,
    arguments: &CleanArgs,
    output: &mut dyn Write,
    signals: &SignalController,
) -> Result<(), Error> {
    if !signals.cancelled() {
        return Ok(());
    }
    let deleted = u64::try_from(report.deleted_session_ids.len()).unwrap_or(u64::MAX);
    output::write_interrupted(deleted, arguments.json, output).map_err(output_error)?;
    Err(Error::Interrupted {
        completed: format!("deleting {deleted} sessions in committed batches"),
    })
}

fn stop_before_mutation_if_cancelled(signals: &SignalController) -> Result<(), Error> {
    if signals.cancelled() {
        Err(Error::Interrupted {
            completed: "zero database mutations".to_owned(),
        })
    } else {
        Ok(())
    }
}

fn render_apply_impact(
    arguments: &CleanArgs,
    impact: &Impact,
    output: &mut dyn Write,
) -> Result<(), Error> {
    if arguments.json {
        Ok(())
    } else {
        impact::write_human(&impact.summary, ReportMode::Pending, output, report_style())
            .map_err(output_error)
    }
}

/// Resolves the styling used by every human renderer this command writes.
fn report_style() -> Style {
    Style::resolve(
        io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
    )
}

fn ensure_confirmed<R>(
    cli: &Cli,
    arguments: &CleanArgs,
    input: &mut R,
    output: &mut dyn Write,
    runtime: RuntimeContext,
    impact: &Impact,
) -> Result<(), Error>
where
    R: BufRead + ?Sized,
{
    let details = format!(
        "Delete {} sessions and {} of attributable data",
        impact.summary.total_session_count,
        format::bytes(impact.summary.total_bytes)
    );
    let escalation = warrants_escalation(
        impact.summary.total_session_count,
        impact.summary.database_session_count,
    )
    .then(|| {
        format!(
            "This deletes {} of the {} sessions in the database.",
            impact.summary.total_session_count, impact.summary.database_session_count
        )
    });
    let decision = confirm(
        &ImpactSummary {
            operation: "Clean",
            details: &details,
            escalation: escalation.as_deref(),
        },
        ConfirmationOptions {
            stdin_is_terminal: runtime.stdin_is_terminal,
            stdout_is_terminal: runtime.stdout_is_terminal,
            json: arguments.json,
            dangerously_skip_confirm: cli.dangerously_skip_confirm,
        },
        input,
        output,
    )
    .map_err(output_error)?;
    match decision {
        ConfirmationDecision::Proceed => Ok(()),
        ConfirmationDecision::Refuse(reason) => Err(refusal_error("clean", "deleting", reason)),
    }
}

/// Turns a refusal into the message the operator should read.
///
/// Declining is a decision and reads as one. An unusable stream is a usage problem, so it names
/// the flag that makes the command work unattended.
fn refusal_error(command: &str, effect: &str, reason: RefusalReason) -> Error {
    match reason {
        RefusalReason::Declined => Error::Canceled {
            reason: format!("{command} canceled; nothing was changed"),
        },
        RefusalReason::Unanswered => Error::Canceled {
            reason: format!(
                "{command} canceled after {CONFIRMATION_ATTEMPTS} unrecognized answers; \
                 nothing was changed"
            ),
        },
        RefusalReason::NotInteractive => Error::InvalidArgument {
            argument: "confirmation".to_owned(),
            reason: format!(
                "{command} cannot ask for confirmation without a terminal; \
                 pass --dangerously-skip-confirm, or --dry-run to preview without {effect}"
            ),
        },
    }
}

fn ensure_selector(arguments: &CleanArgs) -> Result<(), Error> {
    if arguments.older_than.is_some()
        || arguments.include.is_some()
        || arguments.exclude.is_some()
        || arguments.larger_than.is_some()
        || arguments.archived
        || arguments.orphans
    {
        Ok(())
    } else {
        Err(Error::InvalidArgument {
            argument: "clean selection".to_owned(),
            reason:
                "supply --older-than, --include, --exclude, --larger-than, --archived, or --orphans"
                    .to_owned(),
        })
    }
}

fn incremental_precondition_error(error: IncrementalVacuumError) -> Error {
    match error {
        IncrementalVacuumError::Sqlite { context, source } => db::sqlite_error(context, source),
        other => Error::ReclaimUnavailable {
            reason: other.to_string(),
        },
    }
}

fn phase(observer: &dyn PhaseObserver, phase: PhaseId) {
    observer.entered(phase);
    info!(phase = %phase, "clean phase entered");
}

pub(super) fn inspect_read_only_phases(
    target: &Target,
    database_path: &Path,
    cli: &Cli,
    arguments: &CleanArgs,
    holder_inspector: &(dyn HolderInspector + Sync),
) -> Result<(crate::analyze::space::FileSpace, bool), Error> {
    let job_count = usize::from(arguments.incremental) + 3;
    parallel::group("clean", job_count, |group| {
        let database = group.spawn("inspecting schema and space", || {
            inspect_database(target, cli.force_schema)
        });
        let holders = group.spawn("scanning for database holders", || {
            Ok(inspect_holder_decision(
                holder_inspector,
                database_path,
                cli,
            ))
        });
        let incremental = arguments.incremental.then(|| {
            group.spawn("checking incremental auto-vacuum", || {
                let database = db::open_read_only(target, ConnectionOptions::default())?;
                check_auto_vacuum(&database).map_err(incremental_precondition_error)
            })
        });
        let selectors = group.spawn("validating selectors", || ensure_selector(arguments));

        join_read_only_phases(database, holders, incremental, selectors)
    })
}

fn join_read_only_phases(
    database: JobHandle<'_, (crate::analyze::space::FileSpace, bool)>,
    holders: JobHandle<'_, GateDecision>,
    incremental: Option<JobHandle<'_, ()>>,
    selectors: JobHandle<'_, ()>,
) -> Result<(crate::analyze::space::FileSpace, bool), Error> {
    // Joining every handle before propagating errors lets scoped workers finish while preserving
    // declaration order as the stable error-priority contract.
    let database = database.join();
    let holders = holders.join();
    let incremental = incremental.map(JobHandle::join);
    let selectors = selectors.join();

    let database = database?;
    apply_holder_decision(holders?)?;
    if let Some(incremental) = incremental {
        incremental?;
    }
    selectors?;
    Ok(database)
}

#[cfg(test)]
pub(super) fn inspect_read_only_phases_sequential(
    target: &Target,
    database_path: &Path,
    cli: &Cli,
    arguments: &CleanArgs,
    holder_inspector: &(dyn HolderInspector + Sync),
) -> Result<(crate::analyze::space::FileSpace, bool), Error> {
    let database = inspect_database(target, cli.force_schema)?;
    inspect_holders(holder_inspector, database_path, cli)?;
    if arguments.incremental {
        let connection = db::open_read_only(target, ConnectionOptions::default())?;
        check_auto_vacuum(&connection).map_err(incremental_precondition_error)?;
    }
    ensure_selector(arguments)?;
    Ok(database)
}

fn inspect_database(
    target: &Target,
    force_schema: bool,
) -> Result<(crate::analyze::space::FileSpace, bool), Error> {
    let database = db::open_read_only(target, ConnectionOptions::default())?;
    db::schema::inspect(database.connection(), force_schema)?;
    let file_space = crate::analyze::space::analyze(&database)?.file;
    Ok((file_space, database.capabilities().hard_links))
}

#[cfg(test)]
fn inspect_holders(
    holder_inspector: &(dyn HolderInspector + Sync),
    database_path: &Path,
    cli: &Cli,
) -> Result<(), Error> {
    apply_holder_decision(inspect_holder_decision(
        holder_inspector,
        database_path,
        cli,
    ))
}

fn inspect_holder_decision(
    holder_inspector: &(dyn HolderInspector + Sync),
    database_path: &Path,
    cli: &Cli,
) -> GateDecision {
    inspect_and_decide(
        holder_inspector,
        database_path,
        CommandMode::Clean {
            dry_run: cli.dry_run,
        },
        cli.force,
    )
    .1
}

fn apply_holder_decision(holder_decision: GateDecision) -> Result<(), Error> {
    if matches!(holder_decision, GateDecision::Warn) {
        warn!("database holder state requires attention");
    }
    holder_decision.into_result()
}

fn database_target(cli: &Cli) -> Result<Target, Error> {
    let environment = Environment::from_iter(std::env::vars_os());
    paths::database_target(
        &environment,
        DatabaseOptions {
            explicit: cli.db.as_deref(),
            channel: cli.channel.as_deref(),
            platform: current_platform()?,
        },
    )
}

fn file_path(target: &Target) -> Result<&Path, Error> {
    match target {
        Target::File(path) => Ok(path),
        Target::Memory => Err(Error::InvalidArgument {
            argument: "--db".to_owned(),
            reason: "clean requires a file-backed database".to_owned(),
        }),
    }
}

fn derived_paths(database_path: &Path) -> Result<DerivedPaths, Error> {
    database_path
        .parent()
        .map(paths::derived_paths)
        .ok_or_else(|| Error::InvalidArgument {
            argument: "--db".to_owned(),
            reason: "database path has no parent directory".to_owned(),
        })
}

fn current_platform() -> Result<Platform, Error> {
    match std::env::consts::OS {
        "linux" => Ok(Platform::Linux),
        "macos" => Ok(Platform::MacOs),
        "windows" => Ok(Platform::Windows),
        platform => Err(Error::UnsupportedPlatform {
            platform: platform.to_owned(),
        }),
    }
}

#[cfg(target_os = "linux")]
fn platform_inspector() -> Box<dyn HolderInspector + Sync> {
    Box::new(crate::safety::holders::linux::LinuxHolderInspector::default())
}

#[cfg(target_os = "macos")]
fn platform_inspector() -> Box<dyn HolderInspector + Sync> {
    Box::new(crate::safety::holders::macos::MacosHolderInspector)
}

#[cfg(windows)]
fn platform_inspector() -> Box<dyn HolderInspector + Sync> {
    Box::new(crate::safety::holders::windows::WindowsHolderInspector)
}

fn output_error(source: io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<stdout>"),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incremental_precondition_busy_maps_to_database_busy_exit_five() {
        let source = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );

        let error = incremental_precondition_error(IncrementalVacuumError::Sqlite {
            context: "checking incremental vacuum precondition",
            source,
        });

        assert!(matches!(error, Error::DatabaseBusy { .. }));
        assert_eq!(error.exit_code(), 5);
    }
}
