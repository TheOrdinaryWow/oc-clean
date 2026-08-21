use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};
use thiserror::Error;

use super::{database, schema_ddl, FixtureConfig, FixtureResult, BASE_TIME_MS};

const MIN_DISK_RESERVE_BYTES: u64 = 256_000_000;
const SESSION_BATCH_LIMIT: usize = 64;
const ESTIMATED_SESSION_BYTES: u64 = 700_000;

const PART_SIZE_DISTRIBUTION: &[(usize, usize)] = &[
    (60, 1_024),
    (25, 4_096),
    (10, 16_384),
    (4, 65_536),
    (1, 262_144),
];

#[derive(Debug, Error)]
pub enum LargeFixtureError {
    #[error(
        "large fixture needs {required_bytes} bytes including reserve, but only {available_bytes} bytes are available"
    )]
    InsufficientDiskSpace {
        requested_bytes: u64,
        required_bytes: u64,
        available_bytes: u64,
    },
}

#[derive(Clone, Debug)]
pub struct LargeFixtureReport {
    pub target_size_bytes: u64,
    pub achieved_size_bytes: u64,
    pub project_count: usize,
    pub session_count: usize,
    pub message_count: usize,
    pub part_count: usize,
    pub generation_time: Duration,
}

pub(super) fn ensure_capacity(directory: &Path, target_size_bytes: u64) -> FixtureResult<()> {
    let available_bytes = fs2::available_space(directory)?;
    let reserve_bytes = (target_size_bytes / 10).max(MIN_DISK_RESERVE_BYTES);
    let required_bytes = target_size_bytes.saturating_add(reserve_bytes);
    if required_bytes > available_bytes {
        return Err(LargeFixtureError::InsufficientDiskSpace {
            requested_bytes: target_size_bytes,
            required_bytes,
            available_bytes,
        }
        .into());
    }
    Ok(())
}

pub(super) fn populate(
    database_path: &Path,
    config: &FixtureConfig,
    target_size_bytes: u64,
) -> FixtureResult<LargeFixtureReport> {
    let started = Instant::now();
    let mut connection = Connection::open(database_path)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "synchronous", "OFF")?;
    connection.execute_batch(schema_ddl(config.shape))?;
    database::populate(&mut connection, config)?;

    let payloads = payloads();
    let mut session_count = 0;
    let lower_bound = target_size_bytes.saturating_mul(98) / 100;
    loop {
        let achieved_size_bytes = fs::metadata(database_path)?.len();
        if achieved_size_bytes >= lower_bound {
            break;
        }
        let remaining_bytes = lower_bound - achieved_size_bytes;
        let batch_size = usize::try_from(
            remaining_bytes
                .div_ceil(ESTIMATED_SESSION_BYTES)
                .clamp(1, SESSION_BATCH_LIMIT as u64),
        )?;
        insert_session_batch(
            &mut connection,
            config,
            &payloads,
            session_count,
            batch_size,
        )?;
        session_count += batch_size;
    }
    drop(connection);

    let achieved_size_bytes = fs::metadata(database_path)?.len();
    Ok(LargeFixtureReport {
        target_size_bytes,
        achieved_size_bytes,
        project_count: config.project_count,
        session_count,
        message_count: session_count * config.messages_per_session,
        part_count: session_count * config.messages_per_session * config.parts_per_message,
        generation_time: started.elapsed(),
    })
}

fn payloads() -> Vec<String> {
    PART_SIZE_DISTRIBUTION
        .iter()
        .map(|(_, size)| format!(r#"{{"type":"tool","output":"{}"}}"#, "x".repeat(*size)))
        .collect()
}

fn payload_index(part_index: usize) -> usize {
    let sample = part_index % 100;
    let mut boundary = 0;
    PART_SIZE_DISTRIBUTION
        .iter()
        .position(|(weight, _)| {
            boundary += weight;
            sample < boundary
        })
        .expect("distribution weights cover every percentile")
}

fn insert_session_batch(
    connection: &mut Connection,
    config: &FixtureConfig,
    payloads: &[String],
    first_session: usize,
    session_count: usize,
) -> FixtureResult<()> {
    let transaction = connection.transaction()?;
    for session_index in first_session..first_session + session_count {
        insert_session(&transaction, config, payloads, session_index)?;
    }
    transaction.commit()?;
    Ok(())
}

fn insert_session(
    connection: &Connection,
    config: &FixtureConfig,
    payloads: &[String],
    session_index: usize,
) -> FixtureResult<()> {
    let session_id = format!("ses_large_{session_index}");
    let project_index = session_index % config.project_count;
    let project_id = format!("project-{project_index}");
    let workspace_id = format!("workspace-{project_index}");
    let directory = format!("/fixture/{project_id}");
    let created = BASE_TIME_MS - i64::try_from(session_index % 31_536_000)? * 1_000;
    connection.execute(
        "INSERT INTO session (id,project_id,workspace_id,parent_id,slug,directory,path,title,version,time_created,time_updated,time_archived) VALUES (?1,?2,?3,NULL,?1,?4,?4,?1,'1.18.19',?5,?5,NULL)",
        params![session_id, project_id, workspace_id, directory, created],
    )?;

    for message_index in 0..config.messages_per_session {
        let message_id = format!("msg-{session_id}-{message_index}");
        connection.execute(
            "INSERT INTO message VALUES (?1,?2,?3,?3,'{}')",
            params![message_id, session_id, created],
        )?;
        connection.execute(
            "INSERT INTO session_message (id,session_id,type,seq,time_created,time_updated,data) VALUES (?1,?2,'message',?3,?4,?4,'{}')",
            params![message_id, session_id, i64::try_from(message_index)?, created],
        )?;
        for part_index in 0..config.parts_per_message {
            let global_part_index = (session_index * config.messages_per_session + message_index)
                * config.parts_per_message
                + part_index;
            connection.execute(
                "INSERT INTO part VALUES (?1,?2,?3,?4,?4,?5)",
                params![
                    format!("part-{message_id}-{part_index}"),
                    message_id,
                    session_id,
                    created,
                    &payloads[payload_index(global_part_index)]
                ],
            )?;
        }
    }

    connection.execute(
        "INSERT INTO session_input VALUES (?1,?2,'fixture','immediate',1,NULL,?3)",
        params![format!("input-{session_id}"), session_id, created],
    )?;
    connection.execute(
        "INSERT INTO session_context_epoch VALUES (?1,'{}','{}',0)",
        [&session_id],
    )?;
    connection.execute(
        "INSERT INTO session_share VALUES (?1,?2,'secret',?3,?4,?4)",
        params![
            session_id,
            format!("share-{session_id}"),
            format!("https://example.com/{session_id}"),
            created
        ],
    )?;
    connection.execute(
        "INSERT INTO todo VALUES (?1,'fixture','pending','medium',0,?2,?2)",
        params![session_id, created],
    )?;
    connection.execute(
        "INSERT INTO event_sequence VALUES (?1,1,NULL)",
        [&session_id],
    )?;
    connection.execute(
        "INSERT INTO event VALUES (?1,?2,1,'session.created','{}')",
        params![format!("event-{session_id}"), session_id],
    )?;
    Ok(())
}
