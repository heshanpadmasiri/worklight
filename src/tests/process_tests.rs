use crate::error::Error;
use crate::process::ProcessState;

use super::support::{at, Fixture};

#[test]
fn lifecycle_is_persisted_and_completion_is_immutable() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let mut process = storage.start_process("build").unwrap();
    assert!(process.id() > 0);
    let destination = process.snapshot().orchestrator.cwd().to_path_buf();

    process.finish(7).unwrap();
    let finished = process.snapshot();
    assert_eq!(finished.exit_status(), Some(7));
    assert_eq!(finished.outcome(), "failed/7");
    assert_eq!(finished.orchestrator.cwd(), destination);
    assert!(matches!(process.finish(7), Err(Error::AlreadyFinished(_))));

    process.acknowledge().unwrap();
    process.acknowledge().unwrap();
    assert!(process.snapshot().acknowledged());
    assert!(matches!(
        process.finish(0),
        Err(Error::AlreadyAcknowledged(_))
    ));
    assert_eq!(process.snapshot().exit_status(), Some(7));
}

#[test]
fn running_cannot_be_acknowledged_and_failed_write_does_not_change_cache() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let mut process = storage.start_process("test").unwrap();
    assert!(matches!(process.acknowledge(), Err(Error::StillRunning(_))));
    assert_eq!(process.snapshot().state, ProcessState::Running);
}

#[test]
fn observations_are_monotonic_and_reject_changed_completion() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let mut process = storage.start_process("test").unwrap();
    let finished = ProcessState::Finished {
        finished_at: at(10_000),
        exit_status: 2,
    };
    process.observe(finished).unwrap();
    process.observe(ProcessState::Running).unwrap();
    assert_eq!(process.snapshot().state, finished);
    let changed = ProcessState::Acknowledged {
        finished_at: at(10_000),
        exit_status: 3,
    };
    assert!(matches!(
        process.observe(changed),
        Err(Error::CorruptData(_))
    ));
    assert_eq!(process.snapshot().state, finished);
}

#[test]
fn snapshot_elapsed_and_outcome_need_no_refresh() {
    let fixture = Fixture::new();
    let storage = fixture.storage();
    let process = storage.start_process("clock").unwrap();
    let snapshot = process.snapshot();
    assert_eq!(snapshot.outcome(), "running");
    assert!(snapshot.elapsed(snapshot.started_at).is_zero());
}
