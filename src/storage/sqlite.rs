use std::cell::{Ref, RefCell};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{params, params_from_iter, Connection, OpenFlags, Row, TransactionBehavior};

use crate::error::Error;
use crate::orchestrator::Orchestrator;
use crate::process::{from_millis, to_millis, ProcessRun, ProcessState};
use crate::storage::Storage;

const SCHEMA_VERSION: i64 = 2;
const BUSY_TIMEOUT_MS: u64 = 5_000;
const STATE_CHUNK: usize = 900;
const SCHEMA: &str = r#"
CREATE TABLE shells (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    cwd TEXT NOT NULL
);
CREATE TABLE tmux (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    cwd TEXT NOT NULL,
    pane TEXT NOT NULL
);
CREATE TABLE processes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    label TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    running INTEGER NOT NULL DEFAULT 1 CHECK (running IN (0, 1)),
    finished INTEGER NOT NULL DEFAULT 0 CHECK (finished IN (0, 1)),
    acked INTEGER NOT NULL DEFAULT 0 CHECK (acked IN (0, 1)),
    finished_at INTEGER,
    exit_status INTEGER CHECK (exit_status BETWEEN 0 AND 255),
    shell INTEGER NOT NULL CHECK (shell IN (0, 1)),
    tmux INTEGER NOT NULL CHECK (tmux IN (0, 1)),
    orchestrator_id INTEGER NOT NULL,
    CHECK (running + finished + acked = 1),
    CHECK (shell + tmux = 1),
    CHECK ((running = 1 AND finished_at IS NULL AND exit_status IS NULL)
        OR (running = 0 AND finished_at IS NOT NULL AND exit_status IS NOT NULL))
);
CREATE INDEX processes_started ON processes(started_at DESC, id DESC);
CREATE INDEX processes_unacked ON processes(started_at DESC, id DESC) WHERE acked = 0;
CREATE INDEX processes_running ON processes(started_at DESC, id DESC) WHERE running = 1;
"#;
const SELECT_PROCESS: &str = "
 p.id,p.label,p.started_at,p.running,p.finished,p.acked,p.finished_at,p.exit_status,
 p.shell,p.tmux,p.orchestrator_id,s.id,s.cwd,t.id,t.cwd,t.pane
 FROM processes p
 LEFT JOIN shells s ON p.shell=1 AND s.id=p.orchestrator_id
 LEFT JOIN tmux t ON p.tmux=1 AND t.id=p.orchestrator_id";

pub(super) struct Database {
    path: PathBuf,
    read_only: bool,
    connection: RefCell<Option<Connection>>,
}
impl Database {
    pub(super) fn open(path: &Path, read_only: bool) -> Result<Self, Error> {
        let database = Self {
            path: path.to_path_buf(),
            read_only,
            connection: RefCell::new(None),
        };
        drop(database.readable()?);
        Ok(database)
    }
    fn readable(&self) -> Result<Ref<'_, Option<Connection>>, Error> {
        if self.connection.borrow().is_none() && self.path.exists() {
            let connection = open_existing(&self.path, self.read_only)?;
            check_schema(&connection, &self.path)?;
            *self.connection.borrow_mut() = Some(connection);
        }
        Ok(self.connection.borrow())
    }
    fn with_writable<T>(
        &self,
        write: impl FnOnce(&mut Connection) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        drop(self.readable()?);
        let mut slot = self.connection.borrow_mut();
        if slot.is_none() {
            if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Error::Database(format!("cannot create {}: {e}", parent.display()))
                })?;
            }
            let connection = Connection::open(&self.path).map_err(|e| {
                Error::Database(format!("cannot create {}: {e}", self.path.display()))
            })?;
            prepare(&connection, false)?;
            *slot = Some(connection);
        }
        let connection = slot.as_mut().expect("connection opened");
        initialize_schema(connection, &self.path)?;
        write(connection)
    }
    fn query_processes(
        &self,
        storage: &Storage,
        suffix: &str,
        values: &[i64],
    ) -> Result<Vec<ProcessRun>, Error> {
        let slot = self.readable()?;
        let Some(connection) = slot.as_ref() else {
            return Ok(Vec::new());
        };
        ensure_initialized(connection, &self.path)?;
        let sql = format!("SELECT {SELECT_PROCESS} {suffix}");
        let mut statement = connection.prepare(&sql).map_err(map_error)?;
        let mut rows = statement
            .query(params_from_iter(values.iter()))
            .map_err(map_error)?;
        let mut result = Vec::new();
        while let Some(row) = rows.next().map_err(map_error)? {
            result.push(decode_process(row, storage)?);
        }
        Ok(result)
    }
    pub(super) fn start_process(
        &self,
        storage: &Storage,
        label: &str,
        cwd: &Path,
        pane: Option<&str>,
    ) -> Result<ProcessRun, Error> {
        self.with_writable(|connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(map_error)?;
            let (shell, tmux, orchestrator_id) = if let Some(pane) = pane {
                if pane.is_empty() { return Err(Error::Invalid("tmux pane is empty".into())); }
                transaction.execute("INSERT INTO tmux(cwd,pane) VALUES(?1,?2)", params![path_text(cwd), pane]).map_err(map_error)?;
                (0, 1, transaction.last_insert_rowid())
            } else {
                transaction.execute("INSERT INTO shells(cwd) VALUES(?1)", params![path_text(cwd)]).map_err(map_error)?;
                (1, 0, transaction.last_insert_rowid())
            };
            if orchestrator_id <= 0 { return Err(Error::Database("SQLite returned an invalid orchestrator id".into())); }
            let started_at = to_millis(SystemTime::now());
            transaction.execute("INSERT INTO processes(label,started_at,running,finished,acked,shell,tmux,orchestrator_id) VALUES(?1,?2,1,0,0,?3,?4,?5)", params![label, started_at, shell, tmux, orchestrator_id]).map_err(map_error)?;
            let id = transaction.last_insert_rowid();
            if id <= 0 { return Err(Error::Database("SQLite returned an invalid process id".into())); }
            transaction.commit().map_err(map_error)?;
            let orchestrator = if shell == 1 { Orchestrator::restore_shell(storage.clone(), orchestrator_id, cwd.to_path_buf())? } else { Orchestrator::restore_tmux(storage.clone(), orchestrator_id, cwd.to_path_buf(), pane.expect("tmux branch").to_string())? };
            Ok(ProcessRun::hydrate(
                storage.clone(),
                id,
                label.to_string(),
                from_millis(started_at),
                ProcessState::Running,
                orchestrator,
            ))
        })
    }
    pub(super) fn get_process(&self, storage: &Storage, id: i64) -> Result<ProcessRun, Error> {
        self.query_processes(storage, "WHERE p.id=?1", &[id])?
            .into_iter()
            .next()
            .ok_or(Error::NotFound(id))
    }
    pub(super) fn running_process(&self, storage: &Storage) -> Result<Vec<ProcessRun>, Error> {
        self.query_processes(
            storage,
            "WHERE p.running=1 ORDER BY p.started_at DESC,p.id DESC",
            &[],
        )
    }
    pub(super) fn unacked_process(&self, storage: &Storage) -> Result<Vec<ProcessRun>, Error> {
        self.query_processes(
            storage,
            "WHERE p.acked=0 ORDER BY p.started_at DESC,p.id DESC",
            &[],
        )
    }
    pub(super) fn all_process(&self, storage: &Storage) -> Result<Vec<ProcessRun>, Error> {
        self.query_processes(storage, "ORDER BY p.started_at DESC,p.id DESC", &[])
    }
    pub(super) fn latest_process(&self) -> Result<Option<i64>, Error> {
        let slot = self.readable()?;
        let Some(connection) = slot.as_ref() else {
            return Ok(None);
        };
        ensure_initialized(connection, &self.path)?;
        connection
            .query_row("SELECT MAX(id) FROM processes", [], |row| row.get(0))
            .map_err(map_error)
    }
    pub(super) fn range_process(
        &self,
        storage: &Storage,
        after: i64,
        through: i64,
        limit: usize,
    ) -> Result<Vec<ProcessRun>, Error> {
        let limit =
            i64::try_from(limit).map_err(|_| Error::Invalid("range limit is too large".into()))?;
        self.query_processes(
            storage,
            "WHERE p.id>?1 AND p.id<=?2 ORDER BY p.id ASC LIMIT ?3",
            &[after, through, limit],
        )
    }
    pub(super) fn states_process(&self, ids: &[i64]) -> Result<Vec<(i64, ProcessState)>, Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let slot = self.readable()?;
        let Some(connection) = slot.as_ref() else {
            return Err(Error::NotFound(ids[0]));
        };
        ensure_initialized(connection, &self.path)?;
        let mut result = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(STATE_CHUNK) {
            let placeholders = std::iter::repeat("?")
                .take(chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("SELECT id,running,finished,acked,finished_at,exit_status FROM processes WHERE id IN ({placeholders})");
            let mut statement = connection.prepare(&sql).map_err(map_error)?;
            let mut rows = statement
                .query(params_from_iter(chunk.iter()))
                .map_err(map_error)?;
            while let Some(row) = rows.next().map_err(map_error)? {
                result.push((data(row, 0, "process id")?, decode_state_at(row, 1)?));
            }
        }
        for id in ids {
            if !result.iter().any(|(found, _)| found == id) {
                return Err(Error::NotFound(*id));
            }
        }
        Ok(result)
    }
    pub(super) fn finish_process(&self, id: i64, exit_status: u8) -> Result<ProcessState, Error> {
        self.with_writable(|connection| {
            let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(map_error)?;
            let state = read_state(&transaction, id)?;
            match state { ProcessState::Finished { .. } => return Err(Error::AlreadyFinished(id)), ProcessState::Acknowledged { .. } => return Err(Error::AlreadyAcknowledged(id)), ProcessState::Running => {} }
            let finished_at = to_millis(SystemTime::now());
            let changed = transaction.execute("UPDATE processes SET running=0,finished=1,acked=0,finished_at=?2,exit_status=?3 WHERE id=?1 AND running=1", params![id, finished_at, exit_status]).map_err(map_error)?;
            if changed != 1 { return Err(Error::AlreadyFinished(id)); }
            transaction.commit().map_err(map_error)?;
            Ok(ProcessState::Finished { finished_at: from_millis(finished_at), exit_status })
        })
    }
    pub(super) fn acknowledge_process(&self, id: i64) -> Result<ProcessState, Error> {
        self.with_writable(|connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_error)?;
            let state = read_state(&transaction, id)?;
            match state {
                ProcessState::Running => Err(Error::StillRunning(id)),
                state @ ProcessState::Acknowledged { .. } => {
                    transaction.commit().map_err(map_error)?;
                    Ok(state)
                }
                ProcessState::Finished {
                    finished_at,
                    exit_status,
                } => {
                    transaction
                        .execute(
                            "UPDATE processes SET finished=0,acked=1 WHERE id=?1 AND finished=1",
                            [id],
                        )
                        .map_err(map_error)?;
                    transaction.commit().map_err(map_error)?;
                    Ok(ProcessState::Acknowledged {
                        finished_at,
                        exit_status,
                    })
                }
            }
        })
    }
}

fn read_state(connection: &Connection, id: i64) -> Result<ProcessState, Error> {
    let mut statement = connection
        .prepare("SELECT running,finished,acked,finished_at,exit_status FROM processes WHERE id=?1")
        .map_err(map_error)?;
    let mut rows = statement.query([id]).map_err(map_error)?;
    rows.next()
        .map_err(map_error)?
        .map(|row| decode_state_at(row, 0))
        .transpose()?
        .ok_or(Error::NotFound(id))
}
fn decode_process(row: &Row<'_>, storage: &Storage) -> Result<ProcessRun, Error> {
    let id: i64 = data(row, 0, "process id")?;
    let label: String = data(row, 1, "label")?;
    let started: i64 = data(row, 2, "started_at")?;
    let state = decode_state_at(row, 3)?;
    let shell: i64 = data(row, 8, "shell flag")?;
    let tmux: i64 = data(row, 9, "tmux flag")?;
    let oid: i64 = data(row, 10, "orchestrator id")?;
    if !matches!((shell, tmux), (1, 0) | (0, 1)) {
        return Err(Error::CorruptData(format!(
            "process {id} has invalid orchestrator flags"
        )));
    }
    let orchestrator = if shell == 1 {
        let joined: Option<i64> = data(row, 11, "shell id")?;
        if joined != Some(oid) {
            return Err(dangling(id, "shells", oid));
        }
        Orchestrator::restore_shell(
            storage.clone(),
            oid,
            PathBuf::from(data::<String>(row, 12, "shell cwd")?),
        )?
    } else {
        let joined: Option<i64> = data(row, 13, "tmux id")?;
        if joined != Some(oid) {
            return Err(dangling(id, "tmux", oid));
        }
        Orchestrator::restore_tmux(
            storage.clone(),
            oid,
            PathBuf::from(data::<String>(row, 14, "tmux cwd")?),
            data(row, 15, "tmux pane")?,
        )?
    };
    if id <= 0 {
        return Err(Error::CorruptData(format!("invalid process id {id}")));
    }
    if label.is_empty() {
        return Err(Error::CorruptData(format!(
            "process {id} has an empty label"
        )));
    }
    let started_at = from_millis(started);
    let finished_at = match state {
        ProcessState::Running => None,
        ProcessState::Finished { finished_at, .. }
        | ProcessState::Acknowledged { finished_at, .. } => Some(finished_at),
    };
    if finished_at.is_some_and(|finished_at| finished_at < started_at) {
        return Err(Error::CorruptData(format!(
            "process {id} finished before it started"
        )));
    }
    Ok(ProcessRun::hydrate(
        storage.clone(),
        id,
        label,
        started_at,
        state,
        orchestrator,
    ))
}
fn decode_state_at(row: &Row<'_>, offset: usize) -> Result<ProcessState, Error> {
    let running: i64 = data(row, offset, "running")?;
    let finished: i64 = data(row, offset + 1, "finished")?;
    let acked: i64 = data(row, offset + 2, "acked")?;
    let finished_at: Option<i64> = data(row, offset + 3, "finished_at")?;
    let exit: Option<i64> = data(row, offset + 4, "exit_status")?;
    match (running, finished, acked, finished_at, exit) {
        (1, 0, 0, None, None) => Ok(ProcessState::Running),
        (0, 1, 0, Some(time), Some(code @ 0..=255)) => Ok(ProcessState::Finished {
            finished_at: from_millis(time),
            exit_status: code as u8,
        }),
        (0, 0, 1, Some(time), Some(code @ 0..=255)) => Ok(ProcessState::Acknowledged {
            finished_at: from_millis(time),
            exit_status: code as u8,
        }),
        _ => Err(Error::CorruptData(
            "invalid process lifecycle columns".into(),
        )),
    }
}
fn data<T: rusqlite::types::FromSql>(row: &Row<'_>, index: usize, name: &str) -> Result<T, Error> {
    row.get(index)
        .map_err(|e| Error::CorruptData(format!("invalid {name}: {e}")))
}
fn dangling(id: i64, table: &str, target: i64) -> Error {
    Error::CorruptData(format!(
        "process {id} references missing {table} row {target}"
    ))
}
fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
fn open_existing(path: &Path, read_only: bool) -> Result<Connection, Error> {
    let flags = (if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    }) | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let connection = Connection::open_with_flags(path, flags)
        .map_err(|e| Error::Incompatible(format!("cannot open {}: {e}", path.display())))?;
    prepare(&connection, read_only)?;
    Ok(connection)
}
fn prepare(connection: &Connection, read_only: bool) -> Result<(), Error> {
    connection
        .busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(map_error)?;
    if !read_only {
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(map_error)?;
    }
    Ok(())
}
fn schema_version(connection: &Connection) -> Result<i64, Error> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(map_error)
}
fn initialize_schema(connection: &mut Connection, path: &Path) -> Result<(), Error> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    check_schema(&transaction, path)?;
    if schema_version(&transaction)? == 0 {
        transaction.execute_batch(SCHEMA).map_err(map_error)?;
        transaction
            .pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(map_error)?;
    }
    transaction.commit().map_err(map_error)
}
fn ensure_initialized(connection: &Connection, path: &Path) -> Result<(), Error> {
    check_schema(connection, path)?;
    Ok(())
}
fn check_schema(connection: &Connection, path: &Path) -> Result<(), Error> {
    let version = schema_version(connection)?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if version == 0 {
        let count: i64 = connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |row| row.get(0)).map_err(map_error)?;
        if count == 0 {
            return Ok(());
        }
        return Err(Error::Incompatible(format!(
            "{} contains tables but no Worklight schema version",
            path.display()
        )));
    }
    Err(Error::Incompatible(format!(
        "{} has schema version {version}, but this build understands {SCHEMA_VERSION}",
        path.display()
    )))
}
fn map_error(error: rusqlite::Error) -> Error {
    use rusqlite::ffi::ErrorCode;
    if let rusqlite::Error::SqliteFailure(failure, ref message) = error {
        let detail = message.clone().unwrap_or_else(|| error.to_string());
        return match failure.code {
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => Error::Busy(detail),
            ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt => Error::Incompatible(detail),
            ErrorCode::ReadOnly => Error::ReadOnly,
            _ => Error::Database(detail),
        };
    }
    Error::Database(error.to_string())
}
