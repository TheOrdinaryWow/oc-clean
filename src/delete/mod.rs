use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::{db::sqlite_error, error::Error};

pub mod orphans;
pub mod projects;
pub mod sessions;

pub(crate) struct TempIdBatcher<'connection> {
    connection: &'connection Connection,
}

impl<'connection> TempIdBatcher<'connection> {
    pub(crate) fn materialize<'id>(
        connection: &'connection Connection,
        ids: impl IntoIterator<Item = &'id str>,
    ) -> Result<Self, Error> {
        connection
            .execute_batch(
                "DROP TABLE IF EXISTS purge_ids;
                 DROP TABLE IF EXISTS batch_ids;
                 CREATE TEMP TABLE purge_ids(id TEXT PRIMARY KEY);
                 CREATE TEMP TABLE batch_ids(id TEXT PRIMARY KEY);",
            )
            .map_err(|source| sqlite_error("creating temporary deletion id tables", source))?;
        let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Deferred)
            .map_err(|source| sqlite_error("starting deletion id materialization", source))?;
        {
            let mut insert = transaction
                .prepare("INSERT INTO purge_ids(id) VALUES (?1)")
                .map_err(|source| sqlite_error("preparing deletion id materialization", source))?;
            for id in ids {
                insert
                    .execute([id])
                    .map_err(|source| sqlite_error("materializing deletion ids", source))?;
            }
        }
        transaction
            .commit()
            .map_err(|source| sqlite_error("committing deletion id materialization", source))?;
        Ok(Self { connection })
    }

    pub(crate) fn begin_batch(&self, batch_size: i64) -> Result<Option<Transaction<'_>>, Error> {
        let has_candidates = self
            .connection
            .query_row("SELECT EXISTS(SELECT 1 FROM purge_ids)", [], |row| {
                row.get::<_, bool>(0)
            })
            .map_err(|source| sqlite_error("checking temporary deletion ids", source))?;
        if !has_candidates {
            return Ok(None);
        }
        let transaction =
            Transaction::new_unchecked(self.connection, TransactionBehavior::Immediate)
                .map_err(|source| sqlite_error("starting immediate deletion batch", source))?;
        transaction
            .execute("DELETE FROM batch_ids", [])
            .map_err(|source| sqlite_error("clearing temporary batch ids", source))?;
        transaction
            .execute(
                "INSERT INTO batch_ids(id)
                 SELECT id FROM purge_ids ORDER BY id LIMIT ?1",
                [batch_size],
            )
            .map_err(|source| sqlite_error("selecting temporary batch ids", source))?;
        Ok(Some(transaction))
    }

    pub(crate) fn remove_completed(transaction: &Transaction<'_>) -> Result<(), Error> {
        transaction
            .execute(
                "DELETE FROM purge_ids WHERE id IN (SELECT id FROM batch_ids)",
                [],
            )
            .map_err(|source| sqlite_error("removing completed temporary deletion ids", source))?;
        transaction
            .execute("DELETE FROM batch_ids", [])
            .map_err(|source| sqlite_error("clearing completed temporary batch ids", source))?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::duplicate_mod, dead_code)]
#[path = "../../tests/support/fixture.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::fixture::{Fixture, FixtureConfig};
    use super::sessions::{DeleteOptions, delete};
    use crate::db::{ConnectionOptions, open_read_write};
    use crate::error::Error;
    use crate::paths::Target;
    use crate::select::predicates::SessionIds;

    #[test]
    fn lock_contention_during_delete_returns_database_busy() {
        let fixture = Fixture::build(&FixtureConfig::default()).expect("fixture should build");
        let database = open_read_write(
            &Target::File(fixture.database_path.clone()),
            ConnectionOptions {
                busy_timeout: Duration::ZERO,
                ..ConnectionOptions::default()
            },
        )
        .expect("fixture should open read-write");
        let lock_holder = fixture.connect().expect("lock holder should connect");
        lock_holder
            .execute_batch("BEGIN IMMEDIATE")
            .expect("lock holder should acquire a write lock");
        let selected = fixture.session_ids[..1]
            .iter()
            .cloned()
            .collect::<SessionIds>();

        let error = delete(
            &database,
            &selected,
            DeleteOptions {
                batch_size: 1,
                batch_time_limit: Duration::from_secs(30),
            },
        )
        .expect_err("delete should report lock contention");

        assert!(matches!(error, Error::DatabaseBusy { .. }));
        assert_eq!(error.exit_code(), 5);
    }
}
