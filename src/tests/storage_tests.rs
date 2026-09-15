use std::sync::{Arc, Barrier};

use rusqlite::{params, Connection};

use crate::error::Error;
use crate::process::ProcessState;
use crate::storage::Storage;

use super::support::Fixture;

#[test]
fn missing_database_reads_empty_and_lookup_is_not_found() {
    let fixture = Fixture::new();
    let storage = fixture.readonly();
    assert!(storage.all_process().unwrap().is_empty());
    assert_eq!(storage.latest_process().unwrap(), None);
    assert!(matches!(storage.get_process(1), Err(Error::NotFound(1))));
    assert!(!fixture.path().exists());
}

#[test]
fn ids_ordering_filters_and_ranges_are_distinct() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let first = storage.start_process("one").unwrap().id();
    let mut second = storage.start_process("two").unwrap();
    let second_id = second.id();
    assert!(second_id > first);
    second.finish(0).unwrap();

    assert_eq!(
        storage
            .running_process()
            .unwrap()
            .iter()
            .map(|p| p.id())
            .collect::<Vec<_>>(),
        vec![first]
    );
    assert_eq!(storage.unacked_process().unwrap().len(), 2);
    assert_eq!(storage.latest_process().unwrap(), Some(second_id));
    assert_eq!(
        storage
            .range_process(0, second_id, 20)
            .unwrap()
            .iter()
            .map(|p| p.id())
            .collect::<Vec<_>>(),
        vec![first, second_id]
    );
    assert!(matches!(
        storage.range_process(0, second_id, 0),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn schema_has_new_lifecycle_and_partial_indexes() {
    let fixture = Fixture::new();
    fixture.storage().start_process("schema").unwrap();
    let connection = Connection::open(fixture.path()).unwrap();
    let columns: Vec<String> = connection
        .prepare("PRAGMA table_info(processes)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        columns,
        vec![
            "id",
            "label",
            "started_at",
            "running",
            "finished",
            "acked",
            "finished_at",
            "exit_status",
            "shell",
            "tmux",
            "orchestrator_id"
        ]
    );
    let indexes: Vec<String> = connection.prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='processes' AND sql IS NOT NULL ORDER BY name").unwrap().query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(
        indexes,
        vec![
            "processes_collectible",
            "processes_running",
            "processes_shell_orchestrator",
            "processes_started",
            "processes_tmux_orchestrator",
            "processes_unacked"
        ]
    );
}

#[test]
fn deleting_tracked_entities_removes_their_orchestrators() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let process = storage.start_process("delete process").unwrap();
    let agent = storage.register_agent("pi").unwrap();

    storage.delete_process(process.id()).unwrap();
    storage.delete_agent(agent.id()).unwrap();

    assert!(matches!(
        storage.get_process(process.id()),
        Err(Error::NotFound(id)) if id == process.id()
    ));
    assert!(matches!(
        storage.get_agent(agent.id()),
        Err(Error::AgentNotFound(id)) if id == agent.id()
    ));
    let connection = Connection::open(fixture.path()).unwrap();
    let orchestrators: i64 = connection
        .query_row(
            "SELECT (SELECT count(*) FROM shells) + (SELECT count(*) FROM tmux)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(orchestrators, 0);
}

#[test]
fn stale_collection_uses_thresholds_and_preserves_nonterminal_rows() {
    let fixture = Fixture::new();
    fixture.storage().start_process("still running").unwrap();
    let mut connection = Connection::open(fixture.path()).unwrap();
    let transaction = connection.transaction().unwrap();
    for (id, cwd) in [
        (10, "/old-process"),
        (11, "/new-process"),
        (12, "/old-agent"),
        (13, "/new-agent"),
        (14, "/protected"),
    ] {
        transaction
            .execute("INSERT INTO shells(id,cwd) VALUES(?1,?2)", params![id, cwd])
            .unwrap();
    }
    for index in 0..1_000_i64 {
        let orchestrator = if index < 500 { 10 } else { 11 };
        transaction.execute(
            "INSERT INTO processes(label,started_at,running,finished,acked,finished_at,exit_status,shell,tmux,orchestrator_id)
             VALUES('old process',?1,0,0,1,?1,0,1,0,?2)",
            params![index, orchestrator],
        ).unwrap();
        let orchestrator = if index < 500 { 12 } else { 13 };
        transaction
            .execute(
                "INSERT INTO agents(kind,started_at,status,acked,shell,tmux,orchestrator_id)
             VALUES('pi',?1,5,0,1,0,?2)",
                params![index, orchestrator],
            )
            .unwrap();
    }
    transaction.execute(
        "INSERT INTO processes(label,started_at,running,finished,acked,finished_at,exit_status,shell,tmux,orchestrator_id)
         VALUES('unacknowledged',0,0,1,0,1,0,1,0,14)",
        [],
    ).unwrap();
    transaction
        .execute(
            "INSERT INTO agents(kind,started_at,status,acked,shell,tmux,orchestrator_id)
         VALUES('pi',0,4,1,1,0,14)",
            [],
        )
        .unwrap();
    transaction.commit().unwrap();
    drop(connection);

    Storage::collect_stale(fixture.path()).unwrap();
    let connection = Connection::open(fixture.path()).unwrap();
    assert_eq!(count_where(&connection, "processes", "acked=1"), 1_000);
    assert_eq!(count_where(&connection, "agents", "status=5"), 1_000);
    drop(connection);

    let connection = Connection::open(fixture.path()).unwrap();
    connection.execute(
        "INSERT INTO processes(label,started_at,running,finished,acked,finished_at,exit_status,shell,tmux,orchestrator_id)
         VALUES('new process',1000,0,0,1,1000,0,1,0,11)",
        [],
    ).unwrap();
    connection
        .execute(
            "INSERT INTO agents(kind,started_at,status,acked,shell,tmux,orchestrator_id)
         VALUES('pi',1000,5,0,1,0,13)",
            [],
        )
        .unwrap();
    drop(connection);

    Storage::collect_stale(fixture.path()).unwrap();
    let connection = Connection::open(fixture.path()).unwrap();
    assert_eq!(count_where(&connection, "processes", "acked=1"), 501);
    assert_eq!(count_where(&connection, "agents", "status=5"), 501);
    assert_eq!(count_where(&connection, "processes", "finished=1"), 1);
    assert_eq!(
        count_where(&connection, "agents", "status=4 AND acked=1"),
        1
    );
    assert_eq!(count_where(&connection, "processes", "running=1"), 1);
    assert_eq!(
        connection
            .query_row(
                "SELECT min(finished_at) FROM processes WHERE acked=1",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        500
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT min(started_at) FROM agents WHERE status=5",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        500
    );
    assert_eq!(count_where(&connection, "shells", "id IN (10,12)"), 0);
    assert_eq!(count_where(&connection, "shells", "id IN (11,13,14)"), 3);
}

#[test]
fn stale_collection_skips_a_busy_database_and_missing_database() {
    let fixture = Fixture::new();
    fixture.storage().start_process("initialize").unwrap();
    let mut blocker = Connection::open(fixture.path()).unwrap();
    blocker
        .execute_batch(
            "WITH RECURSIVE sequence(value) AS (
                SELECT 1 UNION ALL SELECT value+1 FROM sequence WHERE value<1001
             )
             INSERT INTO processes(label,started_at,running,finished,acked,finished_at,exit_status,shell,tmux,orchestrator_id)
             SELECT 'collectible',value,0,0,1,value,0,1,0,1 FROM sequence;",
        )
        .unwrap();
    let transaction = blocker.transaction().unwrap();
    transaction
        .execute("INSERT INTO shells(cwd) VALUES('/lock')", [])
        .unwrap();
    Storage::collect_stale(fixture.path()).unwrap();
    transaction.rollback().unwrap();

    let missing = fixture.path().with_file_name("missing.db");
    Storage::collect_stale(&missing).unwrap();
    assert!(!missing.exists());
}

fn count_where(connection: &Connection, table: &str, predicate: &str) -> i64 {
    connection
        .query_row(
            &format!("SELECT count(*) FROM {table} WHERE {predicate}"),
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn delete_reports_missing_and_read_only_entities() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let id = storage.start_process("readonly delete").unwrap().id();
    assert!(matches!(
        storage.delete_process(id + 1),
        Err(Error::NotFound(missing)) if missing == id + 1
    ));
    assert!(matches!(
        storage.delete_agent(id),
        Err(Error::AgentNotFound(missing)) if missing == id
    ));
    assert!(matches!(
        fixture.readonly().delete_process(id),
        Err(Error::ReadOnly)
    ));
    assert!(storage.get_process(id).is_ok());
}

#[test]
fn read_only_is_a_mutation_backstop() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let id = storage.start_process("readonly").unwrap().id();
    let readonly = fixture.readonly();
    let mut process = readonly.get_process(id).unwrap();
    assert!(matches!(process.finish(0), Err(Error::ReadOnly)));
    assert_eq!(process.snapshot().state, ProcessState::Running);
}

#[test]
fn synchronization_rejects_mixed_storage_instances() {
    let fixture = Fixture::new();
    let first = fixture.storage();
    let id = first.start_process("mixed").unwrap().id();
    let second = fixture.storage();
    let mut entity = second.get_process(id).unwrap();
    assert!(
        matches!(first.sync_process(&mut [&mut entity]), Err(Error::WrongStorage(value)) if value == id)
    );
}

#[test]
fn external_transitions_are_observed_monotonically() {
    let fixture = Fixture::new();
    let first = fixture.storage();
    let id = first.start_process("external").unwrap().id();
    let second = fixture.storage();
    let mut observer = first.get_process(id).unwrap();
    let mut writer = second.get_process(id).unwrap();
    writer.finish(9).unwrap();
    assert!(matches!(
        observer.state().unwrap(),
        ProcessState::Finished { exit_status: 9, .. }
    ));
    writer.acknowledge().unwrap();
    assert!(matches!(
        observer.state().unwrap(),
        ProcessState::Acknowledged { exit_status: 9, .. }
    ));
}

#[test]
fn concurrent_finish_has_one_winner() {
    let fixture = Fixture::new();
    let id = fixture.storage().start_process("race").unwrap().id();
    let path = Arc::new(fixture.path().to_path_buf());
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [3_u8, 4]
        .into_iter()
        .map(|status| {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let storage = Storage::open(&path).unwrap();
                let mut process = storage.get_process(id).unwrap();
                barrier.wait();
                process.finish(status)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(Error::AlreadyFinished(_))))
            .count(),
        1
    );
}

#[test]
fn fresh_schema_is_version_four_with_agent_indexes() {
    let fixture = Fixture::new();
    fixture.storage().start_process("schema").unwrap();
    let connection = Connection::open(fixture.path()).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
    let indexes: Vec<String> = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='agents' AND sql IS NOT NULL ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        indexes,
        vec![
            "agents_active",
            "agents_collectible",
            "agents_shell_orchestrator",
            "agents_started",
            "agents_tmux_orchestrator",
            "agents_unkilled"
        ]
    );
}

#[test]
fn writable_version_two_migrates_without_rewriting_existing_values() {
    let fixture = Fixture::new();
    create_version_two(fixture.path());
    let before = version_two_values(fixture.path());
    let storage = Storage::open(fixture.path()).unwrap();
    assert!(storage.all_agent().unwrap().is_empty());
    let connection = Connection::open(fixture.path()).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(version_two_values(fixture.path()), before);
}

#[test]
fn background_collection_adds_indexes_to_an_over_threshold_version_three_database() {
    let fixture = Fixture::new();
    create_version_three(fixture.path());
    Connection::open(fixture.path())
        .unwrap()
        .execute_batch(
            "WITH RECURSIVE sequence(value) AS (
                SELECT 1 UNION ALL SELECT value+1 FROM sequence WHERE value<1001
             )
             INSERT INTO processes(label,started_at,running,finished,acked,finished_at,exit_status,shell,tmux,orchestrator_id)
             SELECT 'collectible',value,0,0,1,value,0,1,0,7 FROM sequence;",
        )
        .unwrap();

    Storage::collect_stale(fixture.path()).unwrap();

    let connection = Connection::open(fixture.path()).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert_eq!(count_where(&connection, "processes", "id=9"), 1);
    assert_eq!(
        count_where(
            &connection,
            "sqlite_master",
            "type='index' AND name IN (
                'processes_collectible','processes_shell_orchestrator','processes_tmux_orchestrator',
                'agents_collectible','agents_shell_orchestrator','agents_tmux_orchestrator'
            )"
        ),
        6
    );
}

#[test]
fn failed_migration_rolls_back_objects_and_version() {
    let fixture = Fixture::new();
    create_version_two(fixture.path());
    Connection::open(fixture.path())
        .unwrap()
        .execute_batch(
            "CREATE TABLE migration_conflict(value INTEGER);\
             CREATE INDEX agents_unkilled ON migration_conflict(value);",
        )
        .unwrap();
    assert!(matches!(
        Storage::open(fixture.path()),
        Err(Error::Database(_))
    ));
    let connection = Connection::open(fixture.path()).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='agents'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='agents_started'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn concurrent_writable_openers_serialize_version_two_migration() {
    let fixture = Fixture::new();
    create_version_two(fixture.path());
    let path = Arc::new(fixture.path().to_path_buf());
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                Storage::open(&path).unwrap().all_agent().unwrap().len()
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), 0);
    }
    let connection = Connection::open(fixture.path()).unwrap();
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='agents'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn read_only_version_two_requires_migration_and_modifies_nothing() {
    let fixture = Fixture::new();
    create_version_two(fixture.path());
    let before = std::fs::read(fixture.path()).unwrap();
    assert!(matches!(
        Storage::open_read_only(fixture.path()),
        Err(Error::Incompatible(_))
    ));
    assert_eq!(std::fs::read(fixture.path()).unwrap(), before);
}

#[test]
fn ordinary_and_read_only_version_three_access_does_not_add_collection_indexes() {
    let fixture = Fixture::new();
    create_version_three(fixture.path());
    let before = version_two_values(fixture.path());

    Storage::open(fixture.path()).unwrap();
    Storage::open_read_only(fixture.path()).unwrap();

    let connection = Connection::open(fixture.path()).unwrap();
    assert_eq!(version_two_values(fixture.path()), before);
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        count_where(
            &connection,
            "sqlite_master",
            "type='index' AND name IN ('processes_collectible','agents_collectible')"
        ),
        0
    );
}

#[test]
fn version_one_and_unknown_versions_remain_incompatible() {
    for version in [1, 5, 99] {
        let fixture = Fixture::new();
        let connection = Connection::open(fixture.path()).unwrap();
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(connection);
        assert!(matches!(
            Storage::open(fixture.path()),
            Err(Error::Incompatible(_))
        ));
    }
}

#[test]
fn incompatible_database_is_not_replaced() {
    let fixture = Fixture::new();
    let connection = Connection::open(fixture.path()).unwrap();
    connection
        .execute("CREATE TABLE foreign_data(value TEXT)", [])
        .unwrap();
    drop(connection);
    assert!(matches!(
        Storage::open(fixture.path()),
        Err(Error::Incompatible(_))
    ));
    let connection = Connection::open(fixture.path()).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='foreign_data'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
}

fn create_version_three(path: &std::path::Path) {
    create_version_two(path);
    Connection::open(path)
        .unwrap()
        .execute_batch(
            r#"
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
PRAGMA user_version=3;
"#,
        )
        .unwrap();
}

fn create_version_two(path: &std::path::Path) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            r#"
CREATE TABLE shells (id INTEGER PRIMARY KEY AUTOINCREMENT, cwd TEXT NOT NULL);
CREATE TABLE tmux (id INTEGER PRIMARY KEY AUTOINCREMENT, cwd TEXT NOT NULL, pane TEXT NOT NULL);
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
INSERT INTO shells(id,cwd) VALUES(7,'/shell cwd');
INSERT INTO tmux(id,cwd,pane) VALUES(8,'/tmux cwd','%8');
INSERT INTO processes(id,label,started_at,running,finished,acked,finished_at,exit_status,shell,tmux,orchestrator_id)
VALUES(9,'preserve me',123456,0,1,0,123999,17,1,0,7);
PRAGMA user_version=2;
"#,
        )
        .unwrap();
}

type ProcessValues = (
    i64,
    String,
    i64,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    i64,
    i64,
    i64,
);
type VersionTwoValues = (
    Vec<(i64, String)>,
    Vec<(i64, String, String)>,
    Vec<ProcessValues>,
);

fn version_two_values(path: &std::path::Path) -> VersionTwoValues {
    let connection = Connection::open(path).unwrap();
    let shells = connection
        .prepare("SELECT id,cwd FROM shells ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let tmux = connection
        .prepare("SELECT id,cwd,pane FROM tmux ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let processes = connection
        .prepare("SELECT id,label,started_at,running,finished,acked,finished_at,exit_status,shell,tmux,orchestrator_id FROM processes ORDER BY id")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?,
                row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?, row.get(9)?, row.get(10)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    (shells, tmux, processes)
}
