//! A storage-backed agent-harness runtime.

use std::time::{Duration, SystemTime};

use crate::error::Error;
use crate::orchestrator::{self, Orchestrator, OrchestratorSnapshot, SystemEnvironment};
use crate::storage::Storage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub(crate) enum AgentStatus {
    Idle = 1,
    Working = 2,
    Waiting = 3,
    Done = 4,
    Killed = 5,
}

impl AgentStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Done => "done",
            Self::Killed => "killed",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "idle" => Ok(Self::Idle),
            "working" => Ok(Self::Working),
            "waiting" => Ok(Self::Waiting),
            "done" => Ok(Self::Done),
            "killed" => Ok(Self::Killed),
            _ => Err(Error::Invalid(format!("unknown agent status {value:?}"))),
        }
    }

    pub(crate) fn from_database(value: i64) -> Result<Self, Error> {
        match value {
            1 => Ok(Self::Idle),
            2 => Ok(Self::Working),
            3 => Ok(Self::Waiting),
            4 => Ok(Self::Done),
            5 => Ok(Self::Killed),
            _ => Err(Error::CorruptData(format!("invalid agent status {value}"))),
        }
    }

    pub(crate) fn permits(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (Self::Idle, Self::Working | Self::Killed)
                    | (Self::Working, Self::Waiting | Self::Done | Self::Killed)
                    | (Self::Waiting, Self::Working | Self::Killed)
                    | (Self::Done, Self::Working | Self::Killed)
            )
    }

    pub(crate) fn priority(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Idle => 1,
            Self::Done => 2,
            Self::Working => 3,
            Self::Killed => 4,
        }
    }
}

pub(crate) struct AgentRun {
    id: i64,
    kind: String,
    started_at: SystemTime,
    status: AgentStatus,
    acknowledged: bool,
    orchestrator: Orchestrator,
    storage: Storage,
}

impl AgentRun {
    pub(crate) fn create(storage: Storage, kind: &str) -> Result<Self, Error> {
        if kind.trim().is_empty() {
            return Err(Error::Invalid("agent kind is empty".into()));
        }
        let (cwd, pane) = orchestrator::detect(&SystemEnvironment).map_err(Error::Environment)?;
        storage.create_agent(kind, &cwd, pane.as_deref())
    }

    /// Hydrate an entity from values already validated by the storage boundary.
    pub(crate) fn hydrate(
        storage: Storage,
        id: i64,
        kind: String,
        started_at: SystemTime,
        status: AgentStatus,
        acknowledged: bool,
        orchestrator: Orchestrator,
    ) -> Self {
        Self {
            id,
            kind,
            started_at,
            status,
            acknowledged,
            orchestrator,
            storage,
        }
    }

    pub(crate) fn id(&self) -> i64 {
        self.id
    }

    pub(crate) fn started_at(&self) -> SystemTime {
        self.started_at
    }

    pub(crate) fn orchestrator(&self) -> &Orchestrator {
        &self.orchestrator
    }

    pub(crate) fn status(&mut self) -> Result<AgentStatus, Error> {
        self.sync()?;
        Ok(self.status)
    }

    pub(crate) fn set_status(&mut self, status: AgentStatus) -> Result<(), Error> {
        let (status, acknowledged) = self.storage.set_status_agent(self.id, status)?;
        self.observe(status, acknowledged)
    }

    pub(crate) fn acknowledge(&mut self) -> Result<(), Error> {
        let (status, acknowledged) = self.storage.acknowledge_agent(self.id)?;
        self.observe(status, acknowledged)
    }

    pub(crate) fn focus(&mut self, args: &[String]) -> Result<String, Error> {
        let target = self.orchestrator.navigate(args)?;
        let (status, acknowledged) = self.storage.acknowledge_agent_if_done(self.id)?;
        self.observe(status, acknowledged)?;
        Ok(target)
    }

    pub(crate) fn sync(&mut self) -> Result<(), Error> {
        if self.needs_sync() {
            let (status, acknowledged) = self.storage.state_agent(self.id)?;
            self.observe(status, acknowledged)?;
        }
        Ok(())
    }

    pub(crate) fn observe(&mut self, status: AgentStatus, acknowledged: bool) -> Result<(), Error> {
        if acknowledged && status != AgentStatus::Done {
            return Err(Error::CorruptData(format!(
                "agent {} has acknowledged non-done state",
                self.id
            )));
        }

        let reachable = match self.status {
            AgentStatus::Idle => true,
            AgentStatus::Working | AgentStatus::Waiting | AgentStatus::Done => {
                status != AgentStatus::Idle
            }
            AgentStatus::Killed => status == AgentStatus::Killed,
        };
        if !reachable {
            return Err(Error::CorruptData(format!(
                "agent {} moved from {} to unreachable {} state",
                self.id,
                self.status.as_str(),
                status.as_str()
            )));
        }

        self.status = status;
        self.acknowledged = acknowledged;
        Ok(())
    }

    pub(crate) fn needs_sync(&self) -> bool {
        self.status != AgentStatus::Killed
    }

    pub(crate) fn belongs_to(&self, storage: &Storage) -> bool {
        self.storage.same_storage(storage)
    }

    pub(crate) fn snapshot(&self) -> AgentSnapshot {
        AgentSnapshot {
            id: self.id,
            kind: self.kind.clone(),
            started_at: self.started_at,
            status: self.status,
            acknowledged: self.acknowledged,
            orchestrator: self.orchestrator.snapshot(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AgentSnapshot {
    pub(crate) id: i64,
    pub(crate) kind: String,
    pub(crate) started_at: SystemTime,
    pub(crate) status: AgentStatus,
    pub(crate) acknowledged: bool,
    pub(crate) orchestrator: OrchestratorSnapshot,
}

impl AgentSnapshot {
    pub(crate) fn elapsed(&self, now: SystemTime) -> Duration {
        now.duration_since(self.started_at).unwrap_or_default()
    }
}
