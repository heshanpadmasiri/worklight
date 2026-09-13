use std::sync::{Arc, Barrier};

use rusqlite::Connection;

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
            "processes_running",
            "processes_started",
            "processes_unacked"
        ]
    );
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
