//! Concrete storage handle. SQLite details remain private.

mod sqlite;

use std::path::Path;
use std::rc::Rc;

use crate::agent::{AgentRun, AgentStatus};
use crate::error::Error;
use crate::process::{ProcessRun, ProcessState};

#[derive(Clone)]
pub(crate) struct Storage {
    database: Rc<sqlite::Database>,
}

impl Storage {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        Ok(Self {
            database: Rc::new(sqlite::Database::open(path, false)?),
        })
    }
    pub(crate) fn open_read_only(path: &Path) -> Result<Self, Error> {
        Ok(Self {
            database: Rc::new(sqlite::Database::open(path, true)?),
        })
    }
    pub(crate) fn start_process(&self, label: &str) -> Result<ProcessRun, Error> {
        ProcessRun::create(self.clone(), label)
    }

    pub(crate) fn create_process(
        &self,
        label: &str,
        cwd: &Path,
        pane: Option<&str>,
    ) -> Result<ProcessRun, Error> {
        self.database.start_process(self, label, cwd, pane)
    }
    pub(crate) fn get_process(&self, id: i64) -> Result<ProcessRun, Error> {
        if id <= 0 {
            return Err(Error::Invalid(format!("process id {id} must be positive")));
        }
        self.database.get_process(self, id)
    }
    pub(crate) fn running_process(&self) -> Result<Vec<ProcessRun>, Error> {
        self.database.running_process(self)
    }
    pub(crate) fn unacked_process(&self) -> Result<Vec<ProcessRun>, Error> {
        self.database.unacked_process(self)
    }
    pub(crate) fn all_process(&self) -> Result<Vec<ProcessRun>, Error> {
        self.database.all_process(self)
    }
    pub(crate) fn latest_process(&self) -> Result<Option<i64>, Error> {
        self.database.latest_process()
    }
    pub(crate) fn range_process(
        &self,
        after: i64,
        through: i64,
        limit: usize,
    ) -> Result<Vec<ProcessRun>, Error> {
        if after < 0 || through < 0 || through < after || limit == 0 {
            return Err(Error::Invalid("invalid process range".into()));
        }
        self.database.range_process(self, after, through, limit)
    }
    pub(crate) fn sync_process(&self, processes: &mut [&mut ProcessRun]) -> Result<(), Error> {
        for process in processes.iter() {
            if !process.belongs_to(self) {
                return Err(Error::WrongStorage(process.id()));
            }
        }
        let ids: Vec<i64> = processes
            .iter()
            .filter(|p| p.needs_sync())
            .map(|p| p.id())
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let states = self.database.states_process(&ids)?;
        for (id, state) in states {
            if let Some(process) = processes.iter_mut().find(|p| p.id() == id) {
                process.observe(state)?;
            }
        }
        Ok(())
    }
    pub(crate) fn state_process(&self, id: i64) -> Result<ProcessState, Error> {
        self.database
            .states_process(&[id])?
            .into_iter()
            .next()
            .map(|v| v.1)
            .ok_or(Error::NotFound(id))
    }
    pub(crate) fn finish_process(&self, id: i64, exit_status: u8) -> Result<ProcessState, Error> {
        self.database.finish_process(id, exit_status)
    }
    pub(crate) fn acknowledge_process(&self, id: i64) -> Result<ProcessState, Error> {
        self.database.acknowledge_process(id)
    }
    pub(crate) fn register_agent(&self, kind: &str) -> Result<AgentRun, Error> {
        AgentRun::create(self.clone(), kind)
    }
    pub(crate) fn create_agent(
        &self,
        kind: &str,
        cwd: &Path,
        pane: Option<&str>,
    ) -> Result<AgentRun, Error> {
        if kind.trim().is_empty() {
            return Err(Error::Invalid("agent kind is empty".into()));
        }
        self.database.create_agent(self, kind, cwd, pane)
    }
    pub(crate) fn get_agent(&self, id: i64) -> Result<AgentRun, Error> {
        if id <= 0 {
            return Err(Error::Invalid(format!("agent id {id} must be positive")));
        }
        self.database.get_agent(self, id)
    }
    pub(crate) fn active_agent(&self) -> Result<Vec<AgentRun>, Error> {
        self.database.active_agent(self)
    }
    pub(crate) fn unkilled_agent(&self) -> Result<Vec<AgentRun>, Error> {
        self.database.unkilled_agent(self)
    }
    pub(crate) fn all_agent(&self) -> Result<Vec<AgentRun>, Error> {
        self.database.all_agent(self)
    }
    pub(crate) fn latest_agent(&self) -> Result<Option<i64>, Error> {
        self.database.latest_agent()
    }
    pub(crate) fn range_agent(
        &self,
        after: i64,
        through: i64,
        limit: usize,
    ) -> Result<Vec<AgentRun>, Error> {
        if after < 0 || through < 0 || through < after || limit == 0 {
            return Err(Error::Invalid("invalid agent range".into()));
        }
        self.database.range_agent(self, after, through, limit)
    }
    pub(crate) fn sync_agent(&self, agents: &mut [&mut AgentRun]) -> Result<(), Error> {
        for agent in agents.iter() {
            if !agent.belongs_to(self) {
                return Err(Error::AgentWrongStorage(agent.id()));
            }
        }
        let ids: Vec<i64> = agents
            .iter()
            .filter(|agent| agent.needs_sync())
            .map(|agent| agent.id())
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let states = self.database.states_agent(&ids)?;
        for (id, status, acknowledged) in states {
            for agent in agents.iter_mut().filter(|agent| agent.id() == id) {
                agent.observe(status, acknowledged)?;
            }
        }
        Ok(())
    }
    pub(crate) fn state_agent(&self, id: i64) -> Result<(AgentStatus, bool), Error> {
        self.database
            .states_agent(&[id])?
            .into_iter()
            .next()
            .map(|(_, status, acknowledged)| (status, acknowledged))
            .ok_or(Error::AgentNotFound(id))
    }
    pub(crate) fn set_status_agent(
        &self,
        id: i64,
        status: AgentStatus,
    ) -> Result<(AgentStatus, bool), Error> {
        self.database.set_status_agent(id, status)
    }
    pub(crate) fn acknowledge_agent(&self, id: i64) -> Result<(AgentStatus, bool), Error> {
        self.database.acknowledge_agent(id)
    }
    pub(crate) fn acknowledge_agent_if_done(&self, id: i64) -> Result<(AgentStatus, bool), Error> {
        self.database.acknowledge_agent_if_done(id)
    }
    pub(crate) fn same_storage(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.database, &other.database)
    }
}
