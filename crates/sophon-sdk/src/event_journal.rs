// Copyright 2026 OriginGame contributors
// Licensed under the Apache License, Version 2.0.

//! Durable, restart-stable Session event sequencing.
//!
//! Event payloads remain SDK-owned opaque bytes. A Host owns only the
//! transaction and lifecycle boundary: append commits the event and its new
//! inclusive head together, before the Runtime publishes that event.

use rusqlite::OptionalExtension as _;
use std::path::PathBuf;
use std::sync::Mutex;

pub const EVENT_JOURNAL_SCHEMA_MARKER: &str = "sophon-sdk.event-journal";
pub const EVENT_JOURNAL_SCHEMA_VERSION: u32 = 1;
pub const MAX_SESSION_EVENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventJournalStatus {
    Rebuilding,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSessionEvent {
    pub sequence: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventJournalSnapshot {
    pub generation: u64,
    pub status: EventJournalStatus,
    pub inclusive_end_sequence: u64,
    pub retained: Vec<StoredSessionEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventJournalAppend {
    pub session_id: String,
    pub generation: u64,
    pub expected_head: u64,
    pub event: StoredSessionEvent,
    pub capacity: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventJournalCommit {
    Committed,
    Conflict,
    /// The write may have committed. Callers must reconcile through
    /// [`SessionEventJournalStore::snapshot`] and must not allocate another
    /// sequence until they know.
    CommitUnknown,
}

#[derive(Debug, thiserror::Error)]
pub enum EventJournalStoreError {
    #[error("event journal validation failed: {0}")]
    Validation(String),
    #[error("event journal is corrupt: {0}")]
    Corrupt(String),
    #[error("event journal storage failed: {0}")]
    Storage(String),
}

/// One append-only authority for SDK Session events.
///
/// `initialize` creates generation one in `Ready` at the Host's last durable
/// cursor. A nonzero initial head is the one-time adoption boundary for a Host
/// that persisted SDK cursors before this journal existed; it does not invent
/// retained events. `begin_rebuild` is the only operation allowed to replace a
/// generation and leaves it `Rebuilding` until `finish_rebuild` proves native
/// replay completed. `append` atomically inserts one opaque event, advances the
/// head and prunes the retained prefix.
pub trait SessionEventJournalStore: Send + Sync + 'static {
    fn snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<EventJournalSnapshot>, EventJournalStoreError>;

    fn initialize(
        &self,
        session_id: &str,
        inclusive_end_sequence: u64,
    ) -> Result<EventJournalCommit, EventJournalStoreError>;

    fn begin_rebuild(
        &self,
        session_id: &str,
    ) -> Result<EventJournalSnapshot, EventJournalStoreError>;

    fn append(
        &self,
        append: &EventJournalAppend,
    ) -> Result<EventJournalCommit, EventJournalStoreError>;

    fn finish_rebuild(
        &self,
        session_id: &str,
        generation: u64,
        expected_head: u64,
    ) -> Result<EventJournalCommit, EventJournalStoreError>;

    fn delete(&self, session_id: &str) -> Result<EventJournalCommit, EventJournalStoreError>;
}

/// Standalone SQLite reference authority used when a Host does not inject its
/// own transaction boundary.
pub struct LocalSessionEventJournalStore {
    path: PathBuf,
    connection: Mutex<rusqlite::Connection>,
    #[cfg(test)]
    _temporary_root: Option<tempfile::TempDir>,
}

impl std::fmt::Debug for LocalSessionEventJournalStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalSessionEventJournalStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl LocalSessionEventJournalStore {
    pub fn new(session_storage: impl Into<PathBuf>) -> Result<Self, EventJournalStoreError> {
        let root = session_storage.into();
        std::fs::create_dir_all(&root).map_err(storage)?;
        let path = root.join("origin-event-journal.sqlite3");
        let existed = path.exists();
        let connection = rusqlite::Connection::open(&path).map_err(storage)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(storage)?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;
                 PRAGMA foreign_keys=ON;
                 CREATE TABLE IF NOT EXISTS metadata(
                   key TEXT PRIMARY KEY,
                   value TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS journals(
                   session_id TEXT PRIMARY KEY,
                   generation INTEGER NOT NULL CHECK(generation > 0),
                   status INTEGER NOT NULL CHECK(status IN (0,1)),
                   inclusive_end_sequence INTEGER NOT NULL CHECK(inclusive_end_sequence >= 0)
                 );
                 CREATE TABLE IF NOT EXISTS events(
                   session_id TEXT NOT NULL,
                   generation INTEGER NOT NULL CHECK(generation > 0),
                   sequence INTEGER NOT NULL CHECK(sequence > 0),
                   payload BLOB NOT NULL,
                   PRIMARY KEY(session_id,generation,sequence),
                   FOREIGN KEY(session_id) REFERENCES journals(session_id) ON DELETE CASCADE
                 );",
            )
            .map_err(storage)?;
        let metadata_count: u64 = connection
            .query_row("SELECT COUNT(*) FROM metadata", [], |row| row.get(0))
            .map_err(storage)?;
        if metadata_count == 0 {
            if existed {
                return Err(EventJournalStoreError::Storage(
                    "existing event journal has no schema metadata".into(),
                ));
            }
            let transaction = connection.unchecked_transaction().map_err(storage)?;
            transaction
                .execute(
                    "INSERT INTO metadata(key,value) VALUES('schema_marker',?1),('schema_version',?2)",
                    rusqlite::params![
                        EVENT_JOURNAL_SCHEMA_MARKER,
                        EVENT_JOURNAL_SCHEMA_VERSION.to_string()
                    ],
                )
                .map_err(storage)?;
            transaction.commit().map_err(storage)?;
        } else {
            let marker: Option<String> = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='schema_marker'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?;
            let version: Option<String> = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='schema_version'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?;
            if marker.as_deref() != Some(EVENT_JOURNAL_SCHEMA_MARKER)
                || version.as_deref() != Some("1")
                || metadata_count != 2
            {
                return Err(EventJournalStoreError::Storage(
                    "event journal schema marker/version mismatch".into(),
                ));
            }
        }
        Ok(Self {
            path,
            connection: Mutex::new(connection),
            #[cfg(test)]
            _temporary_root: None,
        })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn temporary() -> Result<Self, EventJournalStoreError> {
        let root = tempfile::tempdir().map_err(storage)?;
        let mut store = Self::new(root.path())?;
        store._temporary_root = Some(root);
        Ok(store)
    }
}

impl SessionEventJournalStore for LocalSessionEventJournalStore {
    fn snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<EventJournalSnapshot>, EventJournalStoreError> {
        validate_session_id(session_id)?;
        let connection = self.connection.lock().map_err(poisoned)?;
        read_snapshot(&connection, session_id)
    }

    fn initialize(
        &self,
        session_id: &str,
        inclusive_end_sequence: u64,
    ) -> Result<EventJournalCommit, EventJournalStoreError> {
        validate_session_id(session_id)?;
        validate_sqlite_sequence(inclusive_end_sequence)?;
        let connection = self.connection.lock().map_err(poisoned)?;
        match connection.execute(
            "INSERT INTO journals(session_id,generation,status,inclusive_end_sequence)
             VALUES(?1,1,1,?2)",
            rusqlite::params![session_id, inclusive_end_sequence],
        ) {
            Ok(1) => Ok(EventJournalCommit::Committed),
            Ok(_) => Err(EventJournalStoreError::Storage(
                "event journal initialize changed an impossible row count".into(),
            )),
            Err(error) if is_constraint(&error) => Ok(EventJournalCommit::Conflict),
            Err(error) => Err(storage(error)),
        }
    }

    fn begin_rebuild(
        &self,
        session_id: &str,
    ) -> Result<EventJournalSnapshot, EventJournalStoreError> {
        validate_session_id(session_id)?;
        let mut connection = self.connection.lock().map_err(poisoned)?;
        let transaction = connection.transaction().map_err(storage)?;
        let current: Option<u64> = transaction
            .query_row(
                "SELECT generation FROM journals WHERE session_id=?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        let generation = current.map_or(Ok(1), |value| {
            value.checked_add(1).ok_or_else(generation_overflow)
        })?;
        transaction
            .execute("DELETE FROM events WHERE session_id=?1", [session_id])
            .map_err(storage)?;
        transaction
            .execute(
                "INSERT INTO journals(session_id,generation,status,inclusive_end_sequence)
                 VALUES(?1,?2,0,0)
                 ON CONFLICT(session_id) DO UPDATE SET
                   generation=excluded.generation,
                   status=excluded.status,
                   inclusive_end_sequence=excluded.inclusive_end_sequence",
                rusqlite::params![session_id, generation],
            )
            .map_err(storage)?;
        transaction.commit().map_err(storage)?;
        Ok(EventJournalSnapshot {
            generation,
            status: EventJournalStatus::Rebuilding,
            inclusive_end_sequence: 0,
            retained: Vec::new(),
        })
    }

    fn append(
        &self,
        append: &EventJournalAppend,
    ) -> Result<EventJournalCommit, EventJournalStoreError> {
        validate_append(append)?;
        let mut connection = self.connection.lock().map_err(poisoned)?;
        let transaction = connection.transaction().map_err(storage)?;
        let current: Option<(u64, u64)> = transaction
            .query_row(
                "SELECT generation,inclusive_end_sequence FROM journals WHERE session_id=?1",
                [&append.session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(storage)?;
        if current != Some((append.generation, append.expected_head)) {
            return Ok(EventJournalCommit::Conflict);
        }
        transaction
            .execute(
                "INSERT INTO events(session_id,generation,sequence,payload)
                 VALUES(?1,?2,?3,?4)",
                rusqlite::params![
                    append.session_id,
                    append.generation,
                    append.event.sequence,
                    append.event.bytes
                ],
            )
            .map_err(storage)?;
        let changed = transaction
            .execute(
                "UPDATE journals SET inclusive_end_sequence=?4
                 WHERE session_id=?1 AND generation=?2 AND inclusive_end_sequence=?3",
                rusqlite::params![
                    append.session_id,
                    append.generation,
                    append.expected_head,
                    append.event.sequence
                ],
            )
            .map_err(storage)?;
        if changed != 1 {
            return Ok(EventJournalCommit::Conflict);
        }
        let capacity = u64::try_from(append.capacity).map_err(|_| {
            EventJournalStoreError::Validation("event journal capacity is too large".into())
        })?;
        let prune_through = append.event.sequence.saturating_sub(capacity);
        transaction
            .execute(
                "DELETE FROM events
                 WHERE session_id=?1 AND generation=?2 AND sequence<=?3",
                rusqlite::params![append.session_id, append.generation, prune_through],
            )
            .map_err(storage)?;
        match transaction.commit() {
            Ok(()) => Ok(EventJournalCommit::Committed),
            Err(_) => Ok(EventJournalCommit::CommitUnknown),
        }
    }

    fn finish_rebuild(
        &self,
        session_id: &str,
        generation: u64,
        expected_head: u64,
    ) -> Result<EventJournalCommit, EventJournalStoreError> {
        validate_session_id(session_id)?;
        if generation == 0 {
            return Err(EventJournalStoreError::Validation(
                "event journal generation must be positive".into(),
            ));
        }
        let connection = self.connection.lock().map_err(poisoned)?;
        let changed = connection
            .execute(
                "UPDATE journals SET status=1
                 WHERE session_id=?1 AND generation=?2
                   AND status=0 AND inclusive_end_sequence=?3",
                rusqlite::params![session_id, generation, expected_head],
            )
            .map_err(storage)?;
        Ok(if changed == 1 {
            EventJournalCommit::Committed
        } else {
            EventJournalCommit::Conflict
        })
    }

    fn delete(&self, session_id: &str) -> Result<EventJournalCommit, EventJournalStoreError> {
        validate_session_id(session_id)?;
        let connection = self.connection.lock().map_err(poisoned)?;
        connection
            .execute("DELETE FROM journals WHERE session_id=?1", [session_id])
            .map_err(storage)?;
        Ok(EventJournalCommit::Committed)
    }
}

fn read_snapshot(
    connection: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<EventJournalSnapshot>, EventJournalStoreError> {
    let Some((generation, status, inclusive_end_sequence)) = connection
        .query_row(
            "SELECT generation,status,inclusive_end_sequence FROM journals WHERE session_id=?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u8>(1)?,
                    row.get::<_, u64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(storage)?
    else {
        return Ok(None);
    };
    let status = match status {
        0 => EventJournalStatus::Rebuilding,
        1 => EventJournalStatus::Ready,
        _ => {
            return Err(EventJournalStoreError::Corrupt(
                "invalid journal status".into(),
            ));
        }
    };
    let mut statement = connection
        .prepare(
            "SELECT sequence,payload FROM events
             WHERE session_id=?1 AND generation=?2 ORDER BY sequence",
        )
        .map_err(storage)?;
    let retained = statement
        .query_map(rusqlite::params![session_id, generation], |row| {
            Ok(StoredSessionEvent {
                sequence: row.get(0)?,
                bytes: row.get(1)?,
            })
        })
        .map_err(storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?;
    validate_snapshot(session_id, generation, inclusive_end_sequence, &retained)?;
    Ok(Some(EventJournalSnapshot {
        generation,
        status,
        inclusive_end_sequence,
        retained,
    }))
}

fn validate_session_id(session_id: &str) -> Result<(), EventJournalStoreError> {
    if session_id.is_empty() || session_id.len() > 1024 || session_id.contains('\0') {
        return Err(EventJournalStoreError::Validation(
            "invalid event journal Session identity".into(),
        ));
    }
    Ok(())
}

fn validate_append(append: &EventJournalAppend) -> Result<(), EventJournalStoreError> {
    validate_session_id(&append.session_id)?;
    validate_sqlite_sequence(append.expected_head)?;
    validate_sqlite_sequence(append.event.sequence)?;
    if append.generation == 0
        || append.capacity == 0
        || append.event.sequence
            != append.expected_head.checked_add(1).ok_or_else(|| {
                EventJournalStoreError::Validation("event journal sequence overflow".into())
            })?
        || append.event.bytes.len() > MAX_SESSION_EVENT_BYTES
    {
        return Err(EventJournalStoreError::Validation(
            "invalid event journal append".into(),
        ));
    }
    Ok(())
}

fn validate_snapshot(
    session_id: &str,
    generation: u64,
    inclusive_end_sequence: u64,
    retained: &[StoredSessionEvent],
) -> Result<(), EventJournalStoreError> {
    validate_session_id(session_id).map_err(as_corrupt)?;
    if generation == 0
        || retained
            .iter()
            .any(|event| event.bytes.len() > MAX_SESSION_EVENT_BYTES)
    {
        return Err(EventJournalStoreError::Corrupt(
            "invalid event journal snapshot".into(),
        ));
    }
    let expected_start = inclusive_end_sequence
        .checked_sub(retained.len() as u64)
        .and_then(|value| value.checked_add(1));
    if retained.iter().enumerate().any(|(index, event)| {
        Some(event.sequence) != expected_start.map(|start| start + index as u64)
    }) {
        return Err(EventJournalStoreError::Corrupt(
            "event journal retained suffix is not contiguous".into(),
        ));
    }
    Ok(())
}

fn validate_sqlite_sequence(sequence: u64) -> Result<(), EventJournalStoreError> {
    if sequence > i64::MAX as u64 {
        return Err(EventJournalStoreError::Validation(
            "event journal sequence exceeds the local store range".into(),
        ));
    }
    Ok(())
}

fn storage(error: impl std::fmt::Display) -> EventJournalStoreError {
    EventJournalStoreError::Storage(error.to_string())
}

fn poisoned(error: impl std::fmt::Display) -> EventJournalStoreError {
    EventJournalStoreError::Storage(format!("event journal lock is poisoned: {error}"))
}

fn as_corrupt(error: EventJournalStoreError) -> EventJournalStoreError {
    EventJournalStoreError::Corrupt(error.to_string())
}

fn generation_overflow() -> EventJournalStoreError {
    EventJournalStoreError::Storage("event journal generation overflow".into())
}

fn is_constraint(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(sequence: u64) -> StoredSessionEvent {
        StoredSessionEvent {
            sequence,
            bytes: format!("event-{sequence}").into_bytes(),
        }
    }

    #[test]
    fn local_journal_append_restart_and_rebuild_are_exact() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalSessionEventJournalStore::new(root.path()).unwrap();
        assert_eq!(
            store.initialize("session-1", 0).unwrap(),
            EventJournalCommit::Committed
        );
        assert_eq!(
            store
                .append(&EventJournalAppend {
                    session_id: "session-1".into(),
                    generation: 1,
                    expected_head: 0,
                    event: event(1),
                    capacity: 2,
                })
                .unwrap(),
            EventJournalCommit::Committed
        );
        assert_eq!(
            store
                .append(&EventJournalAppend {
                    session_id: "session-1".into(),
                    generation: 1,
                    expected_head: 1,
                    event: event(2),
                    capacity: 2,
                })
                .unwrap(),
            EventJournalCommit::Committed
        );
        drop(store);

        let reopened = LocalSessionEventJournalStore::new(root.path()).unwrap();
        let snapshot = reopened.snapshot("session-1").unwrap().unwrap();
        assert_eq!(snapshot.generation, 1);
        assert_eq!(snapshot.status, EventJournalStatus::Ready);
        assert_eq!(snapshot.inclusive_end_sequence, 2);
        assert_eq!(snapshot.retained, [event(1), event(2)]);

        let rebuilding = reopened.begin_rebuild("session-1").unwrap();
        assert_eq!(rebuilding.generation, 2);
        assert_eq!(rebuilding.status, EventJournalStatus::Rebuilding);
        assert_eq!(rebuilding.inclusive_end_sequence, 0);
        assert!(rebuilding.retained.is_empty());
        assert_eq!(
            reopened.finish_rebuild("session-1", 2, 0).unwrap(),
            EventJournalCommit::Committed
        );
        assert_eq!(
            reopened.snapshot("session-1").unwrap().unwrap().status,
            EventJournalStatus::Ready
        );
    }

    #[test]
    fn local_journal_prunes_only_a_committed_prefix() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalSessionEventJournalStore::new(root.path()).unwrap();
        store.initialize("session-1", 0).unwrap();
        for sequence in 1..=3 {
            assert_eq!(
                store
                    .append(&EventJournalAppend {
                        session_id: "session-1".into(),
                        generation: 1,
                        expected_head: sequence - 1,
                        event: event(sequence),
                        capacity: 2,
                    })
                    .unwrap(),
                EventJournalCommit::Committed
            );
        }
        let snapshot = store.snapshot("session-1").unwrap().unwrap();
        assert_eq!(snapshot.inclusive_end_sequence, 3);
        assert_eq!(snapshot.retained, [event(2), event(3)]);
    }

    #[test]
    fn local_journal_adopts_a_host_cursor_without_inventing_events() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalSessionEventJournalStore::new(root.path()).unwrap();
        assert_eq!(
            store.initialize("session-1", 41).unwrap(),
            EventJournalCommit::Committed
        );
        let adopted = store.snapshot("session-1").unwrap().unwrap();
        assert_eq!(adopted.inclusive_end_sequence, 41);
        assert!(adopted.retained.is_empty());
        assert_eq!(
            store
                .append(&EventJournalAppend {
                    session_id: "session-1".into(),
                    generation: adopted.generation,
                    expected_head: 41,
                    event: event(42),
                    capacity: 2,
                })
                .unwrap(),
            EventJournalCommit::Committed
        );
        assert_eq!(
            store.snapshot("session-1").unwrap().unwrap().retained,
            [event(42)]
        );
    }
}
