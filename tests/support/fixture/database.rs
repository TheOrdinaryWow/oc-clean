use rusqlite::{params, Connection};

use super::{FixtureConfig, FixtureResult, BASE_TIME_MS};

pub(super) fn populate(
    connection: &mut Connection,
    config: &FixtureConfig,
) -> FixtureResult<Vec<String>> {
    let transaction = connection.transaction()?;
    populate_projects(&transaction, config)?;
    populate_accounts(&transaction)?;
    let sessions = session_tree(config);
    populate_sessions(&transaction, config, &sessions)?;
    populate_orphan_events(&transaction, config.orphan_event_count)?;
    transaction.execute(
        "INSERT INTO data_migration VALUES ('fixture', ?1)",
        [BASE_TIME_MS],
    )?;
    transaction.execute(
        "INSERT INTO migration VALUES ('fixture', ?1)",
        [BASE_TIME_MS],
    )?;
    transaction.commit()?;
    Ok(sessions.into_iter().map(|(id, _)| id).collect())
}

fn populate_projects(connection: &Connection, config: &FixtureConfig) -> FixtureResult<()> {
    for index in 0..config.project_count {
        let id = format!("project-{index}");
        let worktree = format!("/fixture/{id}");
        connection.execute(
            "INSERT INTO project (id,worktree,vcs,name,time_created,time_updated,sandboxes) VALUES (?1,?2,'git',?1,?3,?3,'[]')",
            params![id, worktree, BASE_TIME_MS],
        )?;
        connection.execute(
            "INSERT INTO project_directory (project_id,directory,type,strategy,time_created) VALUES (?1,?2,'worktree','git',?3)",
            params![id, worktree, BASE_TIME_MS],
        )?;
        connection.execute(
            "INSERT INTO workspace (id,type,name,directory,project_id,time_used) VALUES (?1,'local',?1,?2,?3,?4)",
            params![format!("workspace-{index}"), worktree, id, BASE_TIME_MS],
        )?;
        connection.execute(
            "INSERT INTO permission VALUES (?1,?2,'read','*',?3,?3)",
            params![format!("permission-{index}"), id, BASE_TIME_MS],
        )?;
    }
    Ok(())
}

fn populate_accounts(connection: &Connection) -> FixtureResult<()> {
    connection.execute(
        "INSERT INTO account VALUES ('account-0','fixture@example.com','https://example.com','access','refresh',NULL,?1,?1)",
        [BASE_TIME_MS],
    )?;
    connection.execute("INSERT INTO account_state VALUES (1,'account-0',NULL)", [])?;
    connection.execute(
        "INSERT INTO control_account VALUES ('fixture@example.com','https://example.com','access','refresh',NULL,1,?1,?1)",
        [BASE_TIME_MS],
    )?;
    connection.execute(
        "INSERT INTO credential VALUES ('credential-0',NULL,'fixture','secret',NULL,NULL,1,?1,?1)",
        [BASE_TIME_MS],
    )?;
    Ok(())
}

fn session_tree(config: &FixtureConfig) -> Vec<(String, Option<String>)> {
    let mut sessions = (0..config.session_count)
        .map(|index| (format!("ses_{index}"), None))
        .collect::<Vec<_>>();
    let mut parents = sessions
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for depth in 0..config.sub_session_depth {
        let mut children = Vec::new();
        for parent in &parents {
            for child in 0..config.sub_session_fan_out {
                let id = format!("{parent}_d{depth}_c{child}");
                sessions.push((id.clone(), Some(parent.clone())));
                children.push(id);
            }
        }
        parents = children;
    }
    sessions.extend((0..config.dangling_parent_session_count).map(|index| {
        (
            format!("ses_dangling_{index}"),
            Some(format!("ses_missing_{index}")),
        )
    }));
    sessions
}

fn populate_sessions(
    connection: &Connection,
    config: &FixtureConfig,
    sessions: &[(String, Option<String>)],
) -> FixtureResult<()> {
    let denominator = sessions.len().saturating_sub(1).max(1);
    for (index, (id, parent)) in sessions.iter().enumerate() {
        let project = format!("project-{}", index % config.project_count);
        let created = BASE_TIME_MS
            - config.time_span_ms * i64::try_from(index)? / i64::try_from(denominator)?;
        let archived = (index < config.archived_session_count).then_some(created);
        connection.execute(
            "INSERT INTO session (id,project_id,workspace_id,parent_id,slug,directory,path,title,version,time_created,time_updated,time_archived) VALUES (?1,?2,?3,?4,?1,?5,?5,?1,'1.18.19',?6,?6,?7)",
            params![id, project, format!("workspace-{}", index % config.project_count), parent, format!("/fixture/{project}"), created, archived],
        )?;
        populate_session_relations(connection, config, id, created)?;
    }
    Ok(())
}

fn populate_session_relations(
    connection: &Connection,
    config: &FixtureConfig,
    session_id: &str,
    created: i64,
) -> FixtureResult<()> {
    for message_index in 0..config.messages_per_session {
        let message_id = format!("msg-{session_id}-{message_index}");
        connection.execute(
            "INSERT INTO message VALUES (?1,?2,?3,?3,'{}')",
            params![message_id, session_id, created],
        )?;
        connection.execute(
            "INSERT INTO session_message (id,session_id,type,seq,time_created,time_updated,data) VALUES (?1,?2,'message',?3,?4,?4,'{}')",
            params![
                message_id,
                session_id,
                i64::try_from(message_index)?,
                created
            ],
        )?;
        for part_index in 0..config.parts_per_message {
            let data = format!("{{\"blob\":\"{}\"}}", "x".repeat(config.blob_size_per_part));
            connection.execute(
                "INSERT INTO part VALUES (?1,?2,?3,?4,?4,?5)",
                params![
                    format!("part-{message_id}-{part_index}"),
                    message_id,
                    session_id,
                    created,
                    data
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
        [session_id],
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
        [session_id],
    )?;
    connection.execute(
        "INSERT INTO event VALUES (?1,?2,1,'session.created','{}')",
        params![format!("event-{session_id}"), session_id],
    )?;
    Ok(())
}

fn populate_orphan_events(connection: &Connection, count: usize) -> FixtureResult<()> {
    for index in 0..count {
        let aggregate = format!("ses_orphan_{index}");
        connection.execute(
            "INSERT INTO event_sequence VALUES (?1,1,NULL)",
            [&aggregate],
        )?;
        connection.execute(
            "INSERT INTO event VALUES (?1,?2,1,'session.orphan','{}')",
            params![format!("event-orphan-{index}"), aggregate],
        )?;
    }
    Ok(())
}
