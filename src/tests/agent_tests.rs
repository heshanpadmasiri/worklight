use std::path::Path;
use std::sync::{Arc, Barrier};

use rusqlite::{params, Connection};

use crate::agent::AgentStatus;
use crate::error::Error;
use crate::storage::Storage;

use super::support::Fixture;

const STATUSES: [AgentStatus; 5] = [
    AgentStatus::Idle,
    AgentStatus::Working,
    AgentStatus::Waiting,
    AgentStatus::Done,
    AgentStatus::Killed,
];

#[test]
fn status_representations_are_stable_and_exact() {
    for (number, text, status) in [
        (1, "idle", AgentStatus::Idle),
        (2, "working", AgentStatus::Working),
        (3, "waiting", AgentStatus::Waiting),
        (4, "done", AgentStatus::Done),
        (5, "killed", AgentStatus::Killed),
    ] {
        assert_eq!(status as i64, number);
        assert_eq!(status.as_str(), text);
        assert_eq!(AgentStatus::parse(text).unwrap(), status);
        assert_eq!(AgentStatus::from_database(number).unwrap(), status);
    }
    for value in [i64::MIN, -1, 0, 6, i64::MAX] {
        assert!(matches!(
            AgentStatus::from_database(value),
            Err(Error::CorruptData(_))
        ));
    }
    for value in ["", "Idle", " working", "working ", "unknown"] {
        assert!(matches!(AgentStatus::parse(value), Err(Error::Invalid(_))));
    }
}

#[test]
fn direct_transition_table_is_complete() {
    for from in STATUSES {
        for to in STATUSES {
            let expected = from == to
                || matches!(
                    (from, to),
                    (
                        AgentStatus::Idle,
                        AgentStatus::Working | AgentStatus::Killed
                    ) | (
                        AgentStatus::Working,
                        AgentStatus::Waiting | AgentStatus::Done | AgentStatus::Killed
                    ) | (
                        AgentStatus::Waiting,
                        AgentStatus::Working | AgentStatus::Killed
                    ) | (
                        AgentStatus::Done,
                        AgentStatus::Working | AgentStatus::Killed
                    )
                );
            assert_eq!(from.permits(to), expected, "{from:?} -> {to:?}");
        }
    }
    assert_eq!(AgentStatus::Waiting.priority(), 0);
    assert_eq!(AgentStatus::Idle.priority(), 1);
    assert_eq!(AgentStatus::Working.priority(), 2);
    assert_eq!(AgentStatus::Done.priority(), 3);
    assert_eq!(AgentStatus::Killed.priority(), 4);
}

#[test]
fn observations_accept_missed_cycles_and_reject_impossible_state() {
    let fixture = Fixture::new();
    let storage = fixture.storage();

    let mut idle = storage
        .create_agent("pi", Path::new("/idle"), None)
        .unwrap();
    idle.observe(AgentStatus::Done, false).unwrap();

    let mut waiting = storage
        .create_agent("pi", Path::new("/waiting"), None)
        .unwrap();
    waiting.observe(AgentStatus::Working, false).unwrap();
    waiting.observe(AgentStatus::Waiting, false).unwrap();
    waiting.observe(AgentStatus::Done, false).unwrap();

    let mut acknowledged = storage
        .create_agent("pi", Path::new("/done"), None)
        .unwrap();
    acknowledged.observe(AgentStatus::Done, true).unwrap();
    acknowledged.observe(AgentStatus::Waiting, false).unwrap();

    let mut completed_cycle = storage
        .create_agent("pi", Path::new("/cycle"), None)
        .unwrap();
    completed_cycle.observe(AgentStatus::Done, true).unwrap();
    completed_cycle.observe(AgentStatus::Done, false).unwrap();
    completed_cycle.observe(AgentStatus::Done, true).unwrap();

    let old = completed_cycle.snapshot();
    assert!(matches!(
        completed_cycle.observe(AgentStatus::Working, true),
        Err(Error::CorruptData(_))
    ));
    assert_eq!(completed_cycle.snapshot().status, old.status);
    assert_eq!(completed_cycle.snapshot().acknowledged, old.acknowledged);
    assert!(matches!(
        completed_cycle.observe(AgentStatus::Idle, false),
        Err(Error::CorruptData(_))
    ));

    let mut killed = storage
        .create_agent("pi", Path::new("/killed"), None)
        .unwrap();
    killed.observe(AgentStatus::Killed, false).unwrap();
    assert!(!killed.needs_sync());
    assert!(matches!(
        killed.observe(AgentStatus::Working, false),
        Err(Error::CorruptData(_))
    ));
}

#[test]
fn shell_and_tmux_registration_owns_independent_rows_and_ids_can_overlap() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let process = storage
        .create_process("process", Path::new("/process"), None)
        .unwrap();
    let shell = storage
        .create_agent("pi", Path::new("/shell"), None)
        .unwrap();
    let tmux = storage
        .create_agent("pi", Path::new("/tmux"), Some("%42"))
        .unwrap();

    assert_eq!(process.id(), shell.id());
    assert_eq!(shell.snapshot().status, AgentStatus::Idle);
    assert!(!shell.snapshot().acknowledged);
    assert_eq!(shell.orchestrator().kind(), "shell");
    assert_eq!(tmux.orchestrator().kind(), "tmux");
    assert_ne!(
        shell.orchestrator().id(),
        process.orchestrator().id(),
        "shell={}, process={}",
        shell.orchestrator().id(),
        process.orchestrator().id()
    );
    assert_eq!(tmux.snapshot().orchestrator.kind(), "tmux");
    assert!(shell.started_at() <= std::time::SystemTime::now());

    let connection = Connection::open(fixture.path()).unwrap();
    assert_eq!(count(&connection, "shells"), 2);
    assert_eq!(count(&connection, "tmux"), 1);
    assert_eq!(count(&connection, "agents"), 2);
}

#[test]
fn invalid_registration_has_no_partial_insert() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    assert!(matches!(
        storage.register_agent("  "),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        storage.create_agent("  ", Path::new("/tmp"), None),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        storage.create_agent("pi", Path::new("/tmp"), Some("")),
        Err(Error::Invalid(_))
    ));
    let connection = Connection::open(fixture.path()).unwrap();
    assert_eq!(count(&connection, "shells"), 0);
    assert_eq!(count(&connection, "tmux"), 0);
    assert_eq!(count(&connection, "agents"), 0);
}

#[test]
fn failed_agent_insert_rolls_back_its_orchestrator_row() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    storage
        .create_agent("seed", Path::new("/seed"), None)
        .unwrap();
    let connection = Connection::open(fixture.path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_agent_insert BEFORE INSERT ON agents BEGIN SELECT RAISE(ABORT, 'failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        storage.create_agent("pi", Path::new("/tmp"), None),
        Err(Error::Database(_))
    ));
    assert_eq!(count(&connection, "agents"), 1);
    assert_eq!(count(&connection, "shells"), 1);
}

#[test]
fn queries_ranges_and_filters_have_specified_membership() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let mut idle = storage.create_agent("idle", Path::new("/a"), None).unwrap();
    let mut done = storage.create_agent("done", Path::new("/b"), None).unwrap();
    let mut killed = storage
        .create_agent("killed", Path::new("/c"), None)
        .unwrap();
    done.set_status(AgentStatus::Working).unwrap();
    done.set_status(AgentStatus::Done).unwrap();
    done.acknowledge().unwrap();
    killed.set_status(AgentStatus::Killed).unwrap();

    assert_eq!(storage.latest_agent().unwrap(), Some(killed.id()));
    assert_eq!(ids(storage.active_agent().unwrap()), vec![idle.id()]);
    assert_eq!(
        ids(storage.unkilled_agent().unwrap()),
        vec![done.id(), idle.id()]
    );
    assert_eq!(
        ids(storage.all_agent().unwrap()),
        vec![killed.id(), done.id(), idle.id()]
    );
    assert_eq!(
        ids(storage.range_agent(0, killed.id(), 2).unwrap()),
        vec![idle.id(), done.id()]
    );
    assert!(matches!(
        storage.range_agent(0, killed.id(), 0),
        Err(Error::Invalid(_))
    ));
    idle.status().unwrap();
}

#[test]
fn persisted_mutations_enforce_transitions_and_acknowledgment() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let mut agent = storage.create_agent("pi", Path::new("/tmp"), None).unwrap();

    assert!(matches!(
        agent.set_status(AgentStatus::Done),
        Err(Error::InvalidAgentTransition { .. })
    ));
    assert_eq!(
        storage.state_agent(agent.id()).unwrap(),
        (AgentStatus::Idle, false)
    );
    agent.set_status(AgentStatus::Working).unwrap();
    agent.set_status(AgentStatus::Waiting).unwrap();
    assert!(matches!(agent.acknowledge(), Err(Error::AgentNotDone(_))));
    agent.set_status(AgentStatus::Working).unwrap();
    agent.set_status(AgentStatus::Done).unwrap();
    agent.acknowledge().unwrap();
    agent.acknowledge().unwrap();
    assert_eq!(
        storage.state_agent(agent.id()).unwrap(),
        (AgentStatus::Done, true)
    );

    agent.set_status(AgentStatus::Working).unwrap();
    assert_eq!(
        storage.state_agent(agent.id()).unwrap(),
        (AgentStatus::Working, false)
    );
    agent.set_status(AgentStatus::Done).unwrap();
    agent.acknowledge().unwrap();
    agent.set_status(AgentStatus::Killed).unwrap();
    assert_eq!(
        storage.state_agent(agent.id()).unwrap(),
        (AgentStatus::Killed, false)
    );
    assert!(matches!(
        agent.set_status(AgentStatus::Working),
        Err(Error::InvalidAgentTransition { .. })
    ));
    agent.set_status(AgentStatus::Killed).unwrap();
}

#[test]
fn every_allowed_database_transition_commits_expected_state() {
    for from in STATUSES {
        for to in STATUSES {
            if !from.permits(to) {
                continue;
            }
            let fixture = Fixture::new();
            let storage = fixture.storage();
            let agent = storage.create_agent("pi", Path::new("/tmp"), None).unwrap();
            let acknowledged = from == AgentStatus::Done;
            Connection::open(fixture.path())
                .unwrap()
                .execute(
                    "UPDATE agents SET status=?2,acked=?3 WHERE id=?1",
                    params![agent.id(), from as i64, i64::from(acknowledged)],
                )
                .unwrap();
            let observed = storage.set_status_agent(agent.id(), to).unwrap();
            let expected_ack = acknowledged && from == to;
            assert_eq!(observed, (to, expected_ack), "{from:?} -> {to:?}");
            assert_eq!(storage.state_agent(agent.id()).unwrap(), observed);
        }
    }
}

#[test]
fn every_forbidden_database_transition_writes_nothing() {
    for from in STATUSES {
        for to in STATUSES {
            if from.permits(to) {
                continue;
            }
            let fixture = Fixture::new();
            let storage = fixture.storage();
            let agent = storage.create_agent("pi", Path::new("/tmp"), None).unwrap();
            let connection = Connection::open(fixture.path()).unwrap();
            connection
                .execute(
                    "UPDATE agents SET status=?2,acked=0 WHERE id=?1",
                    params![agent.id(), from as i64],
                )
                .unwrap();
            assert!(matches!(
                storage.set_status_agent(agent.id(), to),
                Err(Error::InvalidAgentTransition { .. })
            ));
            assert_eq!(storage.state_agent(agent.id()).unwrap(), (from, false));
        }
    }
}

#[test]
fn idempotent_status_and_acknowledgment_do_not_execute_updates() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let mut agent = storage.create_agent("pi", Path::new("/tmp"), None).unwrap();
    agent.set_status(AgentStatus::Working).unwrap();
    agent.set_status(AgentStatus::Done).unwrap();
    agent.acknowledge().unwrap();
    let connection = Connection::open(fixture.path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_agent_update BEFORE UPDATE ON agents BEGIN SELECT RAISE(ABORT, 'no update'); END;",
        )
        .unwrap();
    assert_eq!(
        storage
            .set_status_agent(agent.id(), AgentStatus::Done)
            .unwrap(),
        (AgentStatus::Done, true)
    );
    assert_eq!(
        storage.acknowledge_agent(agent.id()).unwrap(),
        (AgentStatus::Done, true)
    );
}

#[test]
fn failed_update_leaves_database_and_loaded_state_unchanged() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let mut agent = storage.create_agent("pi", Path::new("/tmp"), None).unwrap();
    let connection = Connection::open(fixture.path()).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_agent_update BEFORE UPDATE ON agents BEGIN SELECT RAISE(ABORT, 'failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        agent.set_status(AgentStatus::Working),
        Err(Error::Database(_))
    ));
    assert_eq!(agent.snapshot().status, AgentStatus::Idle);
    assert_eq!(
        storage.state_agent(agent.id()).unwrap(),
        (AgentStatus::Idle, false)
    );
}

#[test]
fn strict_and_conditional_acknowledgment_cover_all_statuses() {
    for status in STATUSES {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let agent = storage.create_agent("pi", Path::new("/tmp"), None).unwrap();
        Connection::open(fixture.path())
            .unwrap()
            .execute(
                "UPDATE agents SET status=?2,acked=0 WHERE id=?1",
                params![agent.id(), status as i64],
            )
            .unwrap();
        if status == AgentStatus::Done {
            assert_eq!(
                storage.acknowledge_agent(agent.id()).unwrap(),
                (status, true)
            );
            assert_eq!(
                storage.acknowledge_agent_if_done(agent.id()).unwrap(),
                (status, true)
            );
        } else {
            assert!(matches!(
                storage.acknowledge_agent(agent.id()),
                Err(Error::AgentNotDone(_))
            ));
            assert_eq!(
                storage.acknowledge_agent_if_done(agent.id()).unwrap(),
                (status, false)
            );
        }
    }
}

#[test]
fn failed_shell_navigation_does_not_acknowledge_done() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let mut agent = storage.create_agent("pi", Path::new("/tmp"), None).unwrap();
    agent.set_status(AgentStatus::Working).unwrap();
    agent.set_status(AgentStatus::Done).unwrap();
    assert!(matches!(agent.focus(&[]), Err(Error::Navigation(_))));
    assert_eq!(
        storage.state_agent(agent.id()).unwrap(),
        (AgentStatus::Done, false)
    );
}

#[test]
fn batch_sync_updates_all_non_killed_and_rejects_mixed_storage() {
    let fixture = Fixture::new();
    let first = fixture.storage();
    let second = fixture.storage();
    let mut one = first.create_agent("one", Path::new("/one"), None).unwrap();
    let mut two = first.create_agent("two", Path::new("/two"), None).unwrap();
    let mut duplicate_one = first.get_agent(one.id()).unwrap();
    let mut killed = first
        .create_agent("killed", Path::new("/killed"), None)
        .unwrap();
    second
        .set_status_agent(one.id(), AgentStatus::Working)
        .unwrap();
    second
        .set_status_agent(two.id(), AgentStatus::Working)
        .unwrap();
    second
        .set_status_agent(two.id(), AgentStatus::Waiting)
        .unwrap();
    killed.set_status(AgentStatus::Killed).unwrap();
    first
        .sync_agent(&mut [&mut one, &mut two, &mut duplicate_one, &mut killed])
        .unwrap();
    assert_eq!(one.snapshot().status, AgentStatus::Working);
    assert_eq!(duplicate_one.snapshot().status, AgentStatus::Working);
    assert_eq!(two.snapshot().status, AgentStatus::Waiting);
    assert_eq!(killed.snapshot().status, AgentStatus::Killed);

    let mut foreign = second.get_agent(one.id()).unwrap();
    assert!(matches!(
        first.sync_agent(&mut [&mut foreign]),
        Err(Error::AgentWrongStorage(id)) if id == one.id()
    ));
}

#[test]
fn conditional_ack_race_with_reactivation_is_always_unacknowledged() {
    for _ in 0..10 {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let mut agent = storage.create_agent("pi", Path::new("/tmp"), None).unwrap();
        agent.set_status(AgentStatus::Working).unwrap();
        agent.set_status(AgentStatus::Done).unwrap();
        let id = agent.id();
        let path = Arc::new(fixture.path().to_path_buf());
        let barrier = Arc::new(Barrier::new(2));
        let ack_path = Arc::clone(&path);
        let ack_barrier = Arc::clone(&barrier);
        let ack = std::thread::spawn(move || {
            let storage = Storage::open(&ack_path).unwrap();
            ack_barrier.wait();
            storage.acknowledge_agent_if_done(id).unwrap();
        });
        let status_path = Arc::clone(&path);
        let status_barrier = Arc::clone(&barrier);
        let status = std::thread::spawn(move || {
            let storage = Storage::open(&status_path).unwrap();
            status_barrier.wait();
            storage.set_status_agent(id, AgentStatus::Working).unwrap();
        });
        ack.join().unwrap();
        status.join().unwrap();
        assert_eq!(
            storage.state_agent(id).unwrap(),
            (AgentStatus::Working, false)
        );
    }
}

#[test]
fn corrupt_agent_columns_and_missing_children_are_rejected() {
    for mutation in [
        "UPDATE agents SET status=0",
        "UPDATE agents SET acked=2",
        "UPDATE agents SET status=2,acked=1",
        "UPDATE agents SET shell=0,tmux=0",
        "UPDATE agents SET started_at=-1",
        "UPDATE agents SET kind='   '",
    ] {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let id = storage
            .create_agent("pi", Path::new("/tmp"), None)
            .unwrap()
            .id();
        let connection = Connection::open(fixture.path()).unwrap();
        connection
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        connection.execute(mutation, []).unwrap();
        assert!(
            matches!(storage.get_agent(id), Err(Error::CorruptData(_))),
            "{mutation}"
        );
    }

    let fixture = Fixture::new();
    let storage = fixture.storage();
    let agent = storage.create_agent("pi", Path::new("/tmp"), None).unwrap();
    Connection::open(fixture.path())
        .unwrap()
        .execute(
            "DELETE FROM shells WHERE id=?1",
            [agent.orchestrator().id()],
        )
        .unwrap();
    assert!(matches!(
        storage.get_agent(agent.id()),
        Err(Error::CorruptData(_))
    ));

    let fixture = Fixture::new();
    let storage = fixture.storage();
    storage.create_agent("pi", Path::new("/tmp"), None).unwrap();
    let connection = Connection::open(fixture.path()).unwrap();
    connection
        .execute_batch("PRAGMA ignore_check_constraints=ON")
        .unwrap();
    connection.execute("UPDATE agents SET id=0", []).unwrap();
    assert!(matches!(storage.all_agent(), Err(Error::CorruptData(_))));
}

#[test]
fn snapshot_elapsed_is_storage_free() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let agent = storage.create_agent("pi", Path::new("/tmp"), None).unwrap();
    let snapshot = agent.snapshot();
    assert!(snapshot.elapsed(snapshot.started_at).is_zero());
    assert_eq!(snapshot.kind, "pi");
    assert_eq!(snapshot.id, agent.id());
}

fn ids(agents: Vec<crate::agent::AgentRun>) -> Vec<i64> {
    agents.into_iter().map(|agent| agent.id()).collect()
}

fn count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}
