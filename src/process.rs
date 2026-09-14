//! A storage-backed tracked command entity.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::Error;
use crate::orchestrator::{self, Orchestrator, OrchestratorSnapshot, SystemEnvironment};
use crate::storage::Storage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessState {
    Running,
    Finished {
        finished_at: SystemTime,
        exit_status: u8,
    },
    Acknowledged {
        finished_at: SystemTime,
        exit_status: u8,
    },
}

pub(crate) struct ProcessRun {
    id: i64,
    label: String,
    started_at: SystemTime,
    state: ProcessState,
    orchestrator: Orchestrator,
    storage: Storage,
}

impl ProcessRun {
    /// Validate creation input and persist a new process before returning it.
    pub(crate) fn create(storage: Storage, label: &str) -> Result<Self, Error> {
        if label.trim().is_empty() {
            return Err(Error::Invalid("command label is empty".into()));
        }
        let (cwd, pane) = orchestrator::detect(&SystemEnvironment).map_err(Error::Environment)?;
        storage.create_process(label, &cwd, pane.as_deref())
    }

    /// Hydrate an entity from values already validated by the storage boundary.
    /// This constructor performs neither validation nor persistence.
    pub(crate) fn hydrate(
        storage: Storage,
        id: i64,
        label: String,
        started_at: SystemTime,
        state: ProcessState,
        orchestrator: Orchestrator,
    ) -> Self {
        Self {
            id,
            label,
            started_at,
            state,
            orchestrator,
            storage,
        }
    }
    pub(crate) fn id(&self) -> i64 {
        self.id
    }
    #[allow(dead_code)]
    pub(crate) fn label(&self) -> &str {
        &self.label
    }
    pub(crate) fn started_at(&self) -> SystemTime {
        self.started_at
    }
    pub(crate) fn orchestrator(&self) -> &Orchestrator {
        &self.orchestrator
    }
    pub(crate) fn state(&mut self) -> Result<ProcessState, Error> {
        if !matches!(self.state, ProcessState::Acknowledged { .. }) {
            self.sync()?;
        }
        Ok(self.state)
    }
    #[allow(dead_code)]
    pub(crate) fn finished_at(&mut self) -> Result<Option<SystemTime>, Error> {
        if matches!(self.state, ProcessState::Running) {
            self.sync()?;
        }
        Ok(completion(self.state).map(|(time, _)| time))
    }
    #[allow(dead_code)]
    pub(crate) fn exit_status(&mut self) -> Result<Option<u8>, Error> {
        if matches!(self.state, ProcessState::Running) {
            self.sync()?;
        }
        Ok(completion(self.state).map(|(_, status)| status))
    }
    pub(crate) fn finish(&mut self, exit_status: u8) -> Result<(), Error> {
        let observed = self.storage.finish_process(self.id, exit_status)?;
        self.observe(observed)
    }
    pub(crate) fn acknowledge(&mut self) -> Result<(), Error> {
        let observed = self.storage.acknowledge_process(self.id)?;
        self.observe(observed)
    }
    pub(crate) fn focus(&mut self, args: &[String]) -> Result<String, Error> {
        let target = self.orchestrator.navigate(args)?;
        let state = self.state()?;
        if !matches!(state, ProcessState::Running) {
            self.acknowledge()?;
        }
        Ok(target)
    }
    pub(crate) fn sync(&mut self) -> Result<(), Error> {
        if self.needs_sync() {
            let observed = self.storage.state_process(self.id)?;
            self.observe(observed)?;
        }
        Ok(())
    }
    pub(crate) fn snapshot(&self) -> ProcessSnapshot {
        ProcessSnapshot {
            id: self.id,
            label: self.label.clone(),
            started_at: self.started_at,
            state: self.state,
            orchestrator: self.orchestrator.snapshot(),
        }
    }
    pub(crate) fn belongs_to(&self, storage: &Storage) -> bool {
        self.storage.same_storage(storage)
    }
    /// Finished processes still need synchronization so acknowledgment in one
    /// panel is observed by any other open panels.
    pub(crate) fn needs_sync(&self) -> bool {
        !matches!(self.state, ProcessState::Acknowledged { .. })
    }
    pub(crate) fn observe(&mut self, observed: ProcessState) -> Result<(), Error> {
        use ProcessState::{Acknowledged, Finished, Running};
        if let (Some(old), Some(new)) = (completion(self.state), completion(observed)) {
            if old != new {
                return Err(Error::CorruptData(format!(
                    "process {} has inconsistent completion facts",
                    self.id
                )));
            }
        }
        self.state = match (self.state, observed) {
            (Running, value) => value,
            (Finished { .. }, Running) | (Acknowledged { .. }, Running | Finished { .. }) => {
                self.state
            }
            (Finished { .. }, value @ (Finished { .. } | Acknowledged { .. })) => value,
            (Acknowledged { .. }, Acknowledged { .. }) => self.state,
        };
        Ok(())
    }
}

fn completion(state: ProcessState) -> Option<(SystemTime, u8)> {
    match state {
        ProcessState::Running => None,
        ProcessState::Finished {
            finished_at,
            exit_status,
        }
        | ProcessState::Acknowledged {
            finished_at,
            exit_status,
        } => Some((finished_at, exit_status)),
    }
}
#[derive(Debug, Clone)]
pub(crate) struct ProcessSnapshot {
    pub(crate) id: i64,
    pub(crate) label: String,
    pub(crate) started_at: SystemTime,
    pub(crate) state: ProcessState,
    pub(crate) orchestrator: OrchestratorSnapshot,
}
impl ProcessSnapshot {
    pub(crate) fn elapsed(&self, now: SystemTime) -> Duration {
        let end = completion(self.state).map_or(now, |v| v.0);
        end.duration_since(self.started_at).unwrap_or_default()
    }
    pub(crate) fn outcome(&self) -> String {
        match completion(self.state).map(|v| v.1) {
            None => "running".into(),
            Some(0) => "succeeded".into(),
            Some(code) => format!("failed/{code}"),
        }
    }
    pub(crate) fn exit_status(&self) -> Option<u8> {
        completion(self.state).map(|v| v.1)
    }
    pub(crate) fn acknowledged(&self) -> bool {
        matches!(self.state, ProcessState::Acknowledged { .. })
    }
}

pub(crate) fn to_millis(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(delta) => delta.as_millis().min(i64::MAX as u128) as i64,
        Err(_) => 0,
    }
}
pub(crate) fn from_millis(millis: i64) -> SystemTime {
    if millis <= 0 {
        UNIX_EPOCH
    } else {
        UNIX_EPOCH + Duration::from_millis(millis as u64)
    }
}
