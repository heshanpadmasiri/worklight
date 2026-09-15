use std::cell::{Ref, RefCell};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{params, params_from_iter, Connection, OpenFlags, Row, TransactionBehavior};

use crate::agent::{AgentRun, AgentStatus};
use crate::error::Error;
use crate::orchestrator::Orchestrator;
use crate::process::{from_millis, to_millis, ProcessRun, ProcessState};
use crate::storage::Storage;

const SCHEMA_VERSION: i64 = 3;
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

CREATE TABLE agents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK (length(trim(kind)) > 0),
    started_at INTEGER NOT NULL,
    status INTEGER NOT NULL CHECK (status BETWEEN 1 AND 5),
    acked INTEGER NOT NULL DEFAULT 0 CHECK (acked IN (0, 1)),
    shell INTEGER NOT NULL CHECK (shell IN (0, 1)),
    tmux INTEGER NOT NULL CHECK (tmux IN (0, 1)),
    orchestrator_id INTEGER NOT NULL,
    CHECK (shell + tmux = 1),
    CHECK (acked = 0 OR status = 4)
);
CREATE INDEX agents_started ON agents(started_at DESC, id DESC);
CREATE INDEX agents_unkilled ON agents(started_at DESC, id DESC) WHERE status != 5;
CREATE INDEX agents_active ON agents(started_at DESC, id DESC) WHERE acked = 0 AND status != 5;
"#;
const AGENT_SCHEMA: &str = r#"
CREATE TABLE agents (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK (length(trim(kind)) > 0),
    started_at INTEGER NOT NULL,
    status INTEGER NOT NULL CHECK (status BETWEEN 1 AND 5),
    acked INTEGER NOT NULL DEFAULT 0 CHECK (acked IN (0, 1)),
    shell INTEGER NOT NULL CHECK (shell IN (0, 1)),
    tmux INTEGER NOT NULL CHECK (tmux IN (0, 1)),
    orchestrator_id INTEGER NOT NULL,
    CHECK (shell + tmux = 1),
    CHECK (acked = 0 OR status = 4)
);
CREATE INDEX agents_started ON agents(started_at DESC, id DESC);
CREATE INDEX agents_unkilled ON agents(started_at DESC, id DESC) WHERE status != 5;
CREATE INDEX agents_active ON agents(started_at DESC, id DESC) WHERE acked = 0 AND status != 5;
"#;
const SELECT_PROCESS: &str = "
 p.id,p.label,p.started_at,p.running,p.finished,p.acked,p.finished_at,p.exit_status,
 p.shell,p.tmux,p.orchestrator_id,s.id,s.cwd,t.id,t.cwd,t.pane
 FROM processes p
 LEFT JOIN shells s ON p.shell=1 AND s.id=p.orchestrator_id
 LEFT JOIN tmux t ON p.tmux=1 AND t.id=p.orchestrator_id";
const SELECT_AGENT: &str = "
 a.id,a.kind,a.started_at,a.status,a.acked,a.shell,a.tmux,a.orchestrator_id,
 s.id,s.cwd,t.id,t.cwd,t.pane
 FROM agents a
 LEFT JOIN shells s ON a.shell=1 AND s.id=a.orchestrator_id
 LEFT JOIN tmux t ON a.tmux=1 AND t.id=a.orchestrator_id";

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
            let mut connection = open_existing(&self.path, self.read_only)?;
            if self.read_only {
                check_schema(&connection, &self.path)?;
            } else {
                migrate_or_check_schema(&mut connection, &self.path)?;
            }
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
    fn query_agents(
        &self,
        storage: &Storage,
        suffix: &str,
        values: &[i64],
    ) -> Result<Vec<AgentRun>, Error> {
        let slot = self.readable()?;
        let Some(connection) = slot.as_ref() else {
            return Ok(Vec::new());
        };
        ensure_initialized(connection, &self.path)?;
        let sql = format!("SELECT {SELECT_AGENT} {suffix}");
        let mut statement = connection.prepare(&sql).map_err(map_error)?;
        let mut rows = statement
            .query(params_from_iter(values.iter()))
            .map_err(map_error)?;
        let mut result = Vec::new();
        while let Some(row) = rows.next().map_err(map_error)? {
            result.push(decode_agent(row, storage)?);
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

    pub(super) fn delete_process(&self, id: i64) -> Result<(), Error> {
        self.with_writable(|connection| delete_tracked(connection, "processes", id))
    }

    pub(super) fn create_agent(
        &self,
        storage: &Storage,
        kind: &str,
        cwd: &Path,
        pane: Option<&str>,
    ) -> Result<AgentRun, Error> {
        self.with_writable(|connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_error)?;
            let (shell, tmux, orchestrator_id) = if let Some(pane) = pane {
                if pane.is_empty() {
                    return Err(Error::Invalid("tmux pane is empty".into()));
                }
                transaction
                    .execute(
                        "INSERT INTO tmux(cwd,pane) VALUES(?1,?2)",
                        params![path_text(cwd), pane],
                    )
                    .map_err(map_error)?;
                (0, 1, transaction.last_insert_rowid())
            } else {
                transaction
                    .execute(
                        "INSERT INTO shells(cwd) VALUES(?1)",
                        params![path_text(cwd)],
                    )
                    .map_err(map_error)?;
                (1, 0, transaction.last_insert_rowid())
            };
            if orchestrator_id <= 0 {
                return Err(Error::Database(
                    "SQLite returned an invalid orchestrator id".into(),
                ));
            }
            let started_at = to_millis(SystemTime::now());
            transaction
                .execute(
                    "INSERT INTO agents(kind,started_at,status,acked,shell,tmux,orchestrator_id) VALUES(?1,?2,1,0,?3,?4,?5)",
                    params![kind, started_at, shell, tmux, orchestrator_id],
                )
                .map_err(map_error)?;
            let id = transaction.last_insert_rowid();
            if id <= 0 {
                return Err(Error::Database("SQLite returned an invalid agent id".into()));
            }
            transaction.commit().map_err(map_error)?;
            let orchestrator = if shell == 1 {
                Orchestrator::restore_shell(
                    storage.clone(),
                    orchestrator_id,
                    cwd.to_path_buf(),
                )?
            } else {
                Orchestrator::restore_tmux(
                    storage.clone(),
                    orchestrator_id,
                    cwd.to_path_buf(),
                    pane.expect("tmux branch").to_string(),
                )?
            };
            Ok(AgentRun::hydrate(
                storage.clone(),
                id,
                kind.to_string(),
                from_millis(started_at),
                AgentStatus::Idle,
                false,
                orchestrator,
            ))
        })
    }

    pub(super) fn get_agent(&self, storage: &Storage, id: i64) -> Result<AgentRun, Error> {
        self.query_agents(storage, "WHERE a.id=?1", &[id])?
            .into_iter()
            .next()
            .ok_or(Error::AgentNotFound(id))
    }

    pub(super) fn active_agent(&self, storage: &Storage) -> Result<Vec<AgentRun>, Error> {
        self.query_agents(
            storage,
            "WHERE a.acked=0 AND a.status!=5 ORDER BY a.started_at DESC,a.id DESC",
            &[],
        )
    }

    pub(super) fn unkilled_agent(&self, storage: &Storage) -> Result<Vec<AgentRun>, Error> {
        self.query_agents(
            storage,
            "WHERE a.status!=5 ORDER BY a.started_at DESC,a.id DESC",
            &[],
        )
    }

    pub(super) fn all_agent(&self, storage: &Storage) -> Result<Vec<AgentRun>, Error> {
        self.query_agents(storage, "ORDER BY a.started_at DESC,a.id DESC", &[])
    }

    pub(super) fn latest_agent(&self) -> Result<Option<i64>, Error> {
        let slot = self.readable()?;
        let Some(connection) = slot.as_ref() else {
            return Ok(None);
        };
        ensure_initialized(connection, &self.path)?;
        connection
            .query_row("SELECT MAX(id) FROM agents", [], |row| row.get(0))
            .map_err(map_error)
    }

    pub(super) fn range_agent(
        &self,
        storage: &Storage,
        after: i64,
        through: i64,
        limit: usize,
    ) -> Result<Vec<AgentRun>, Error> {
        let limit =
            i64::try_from(limit).map_err(|_| Error::Invalid("range limit is too large".into()))?;
        self.query_agents(
            storage,
            "WHERE a.id>?1 AND a.id<=?2 ORDER BY a.id ASC LIMIT ?3",
            &[after, through, limit],
        )
    }

    pub(super) fn states_agent(&self, ids: &[i64]) -> Result<Vec<(i64, AgentStatus, bool)>, Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let slot = self.readable()?;
        let Some(connection) = slot.as_ref() else {
            return Err(Error::AgentNotFound(ids[0]));
        };
        ensure_initialized(connection, &self.path)?;
        let mut result = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(STATE_CHUNK) {
            let placeholders = std::iter::repeat("?")
                .take(chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("SELECT id,status,acked FROM agents WHERE id IN ({placeholders})");
            let mut statement = connection.prepare(&sql).map_err(map_error)?;
            let mut rows = statement
                .query(params_from_iter(chunk.iter()))
                .map_err(map_error)?;
            while let Some(row) = rows.next().map_err(map_error)? {
                let id = data(row, 0, "agent id")?;
                if id <= 0 {
                    return Err(Error::CorruptData(format!("invalid agent id {id}")));
                }
                let (status, acknowledged) = decode_agent_state_at(row, 1)?;
                result.push((id, status, acknowledged));
            }
        }
        for id in ids {
            if !result.iter().any(|(found, _, _)| found == id) {
                return Err(Error::AgentNotFound(*id));
            }
        }
        Ok(result)
    }

    pub(super) fn set_status_agent(
        &self,
        id: i64,
        next: AgentStatus,
    ) -> Result<(AgentStatus, bool), Error> {
        self.with_writable(|connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_error)?;
            let (current, acknowledged) = read_agent_state(&transaction, id)?;
            if current == next {
                transaction.commit().map_err(map_error)?;
                return Ok((current, acknowledged));
            }
            if !current.permits(next) {
                return Err(Error::InvalidAgentTransition {
                    id,
                    from: current,
                    to: next,
                });
            }
            let changed = transaction
                .execute(
                    "UPDATE agents SET status=?2,acked=0 WHERE id=?1 AND status=?3 AND acked=?4",
                    params![id, next as i64, current as i64, i64::from(acknowledged)],
                )
                .map_err(map_error)?;
            if changed != 1 {
                return Err(Error::Database(format!(
                    "agent {id} changed during status update"
                )));
            }
            transaction.commit().map_err(map_error)?;
            Ok((next, false))
        })
    }

    pub(super) fn acknowledge_agent(&self, id: i64) -> Result<(AgentStatus, bool), Error> {
        self.with_writable(|connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_error)?;
            let (status, acknowledged) = read_agent_state(&transaction, id)?;
            if status != AgentStatus::Done {
                return Err(Error::AgentNotDone(id));
            }
            if acknowledged {
                transaction.commit().map_err(map_error)?;
                return Ok((status, true));
            }
            let changed = transaction
                .execute(
                    "UPDATE agents SET acked=1 WHERE id=?1 AND status=4 AND acked=0",
                    [id],
                )
                .map_err(map_error)?;
            if changed != 1 {
                return Err(Error::Database(format!(
                    "agent {id} changed during acknowledgment"
                )));
            }
            transaction.commit().map_err(map_error)?;
            Ok((status, true))
        })
    }

    pub(super) fn acknowledge_agent_if_done(&self, id: i64) -> Result<(AgentStatus, bool), Error> {
        self.with_writable(|connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_error)?;
            let (status, acknowledged) = read_agent_state(&transaction, id)?;
            if status != AgentStatus::Done || acknowledged {
                transaction.commit().map_err(map_error)?;
                return Ok((status, acknowledged));
            }
            let changed = transaction
                .execute(
                    "UPDATE agents SET acked=1 WHERE id=?1 AND status=4 AND acked=0",
                    [id],
                )
                .map_err(map_error)?;
            if changed != 1 {
                return Err(Error::Database(format!(
                    "agent {id} changed during conditional acknowledgment"
                )));
            }
            transaction.commit().map_err(map_error)?;
            Ok((status, true))
        })
    }

    pub(super) fn delete_agent(&self, id: i64) -> Result<(), Error> {
        self.with_writable(|connection| delete_tracked(connection, "agents", id))
    }
}

fn delete_tracked(connection: &mut Connection, table: &str, id: i64) -> Result<(), Error> {
    debug_assert!(matches!(table, "processes" | "agents"));
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    let sql = format!("SELECT shell,tmux,orchestrator_id FROM {table} WHERE id=?1");
    let (shell, tmux, orchestrator_id): (i64, i64, i64) = transaction
        .query_row(&sql, [id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows if table == "agents" => Error::AgentNotFound(id),
            rusqlite::Error::QueryReturnedNoRows => Error::NotFound(id),
            error => map_error(error),
        })?;
    let changed = transaction
        .execute(&format!("DELETE FROM {table} WHERE id=?1"), [id])
        .map_err(map_error)?;
    if changed != 1 {
        return Err(Error::Database(format!(
            "{table} row {id} changed during deletion"
        )));
    }

    let (orchestrator_table, flag) = match (shell, tmux) {
        (1, 0) => ("shells", "shell"),
        (0, 1) => ("tmux", "tmux"),
        _ => {
            return Err(Error::CorruptData(format!(
                "{table} row {id} has invalid orchestrator flags"
            )))
        }
    };
    let references: i64 = transaction
        .query_row(
            &format!(
                "SELECT (SELECT count(*) FROM processes WHERE {flag}=1 AND orchestrator_id=?1) + \
                 (SELECT count(*) FROM agents WHERE {flag}=1 AND orchestrator_id=?1)"
            ),
            [orchestrator_id],
            |row| row.get(0),
        )
        .map_err(map_error)?;
    if references == 0 {
        transaction
            .execute(
                &format!("DELETE FROM {orchestrator_table} WHERE id=?1"),
                [orchestrator_id],
            )
            .map_err(map_error)?;
    }
    transaction.commit().map_err(map_error)
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
fn read_agent_state(connection: &Connection, id: i64) -> Result<(AgentStatus, bool), Error> {
    let mut statement = connection
        .prepare("SELECT status,acked FROM agents WHERE id=?1")
        .map_err(map_error)?;
    let mut rows = statement.query([id]).map_err(map_error)?;
    rows.next()
        .map_err(map_error)?
        .map(|row| decode_agent_state_at(row, 0))
        .transpose()?
        .ok_or(Error::AgentNotFound(id))
}

fn decode_agent(row: &Row<'_>, storage: &Storage) -> Result<AgentRun, Error> {
    let id: i64 = data(row, 0, "agent id")?;
    let kind: String = data(row, 1, "agent kind")?;
    let started: i64 = data(row, 2, "agent started_at")?;
    let (status, acknowledged) = decode_agent_state_at(row, 3)?;
    let shell: i64 = data(row, 5, "agent shell flag")?;
    let tmux: i64 = data(row, 6, "agent tmux flag")?;
    let oid: i64 = data(row, 7, "agent orchestrator id")?;
    if id <= 0 {
        return Err(Error::CorruptData(format!("invalid agent id {id}")));
    }
    if kind.trim().is_empty() {
        return Err(Error::CorruptData(format!("agent {id} has a blank kind")));
    }
    if started < 0 {
        return Err(Error::CorruptData(format!(
            "agent {id} has invalid started_at"
        )));
    }
    if !matches!((shell, tmux), (1, 0) | (0, 1)) {
        return Err(Error::CorruptData(format!(
            "agent {id} has invalid orchestrator flags"
        )));
    }
    let orchestrator = if shell == 1 {
        let joined: Option<i64> = data(row, 8, "agent shell id")?;
        if joined != Some(oid) {
            return Err(agent_dangling(id, "shells", oid));
        }
        Orchestrator::restore_shell(
            storage.clone(),
            oid,
            PathBuf::from(data::<String>(row, 9, "agent shell cwd")?),
        )?
    } else {
        let joined: Option<i64> = data(row, 10, "agent tmux id")?;
        if joined != Some(oid) {
            return Err(agent_dangling(id, "tmux", oid));
        }
        Orchestrator::restore_tmux(
            storage.clone(),
            oid,
            PathBuf::from(data::<String>(row, 11, "agent tmux cwd")?),
            data(row, 12, "agent tmux pane")?,
        )?
    };
    Ok(AgentRun::hydrate(
        storage.clone(),
        id,
        kind,
        from_millis(started),
        status,
        acknowledged,
        orchestrator,
    ))
}

fn decode_agent_state_at(row: &Row<'_>, offset: usize) -> Result<(AgentStatus, bool), Error> {
    let status = AgentStatus::from_database(data(row, offset, "agent status")?)?;
    let acknowledged: i64 = data(row, offset + 1, "agent acknowledged flag")?;
    match (status, acknowledged) {
        (_, 0) => Ok((status, false)),
        (AgentStatus::Done, 1) => Ok((status, true)),
        (_, 1) => Err(Error::CorruptData(
            "agent has acknowledged non-done state".into(),
        )),
        _ => Err(Error::CorruptData("invalid agent acknowledged flag".into())),
    }
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
fn agent_dangling(id: i64, table: &str, target: i64) -> Error {
    Error::CorruptData(format!(
        "agent {id} references missing {table} row {target}"
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
fn migrate_or_check_schema(connection: &mut Connection, path: &Path) -> Result<(), Error> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_error)?;
    match schema_version(&transaction)? {
        SCHEMA_VERSION => {}
        2 => {
            transaction.execute_batch(AGENT_SCHEMA).map_err(map_error)?;
            transaction
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(map_error)?;
        }
        _ => check_schema(&transaction, path)?,
    }
    transaction.commit().map_err(map_error)
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
    if version == 2 {
        return Err(Error::Incompatible(format!(
            "{} has schema version 2 and requires migration to version {SCHEMA_VERSION}",
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
