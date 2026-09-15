use crate::agent::AgentStatus;
use crate::orchestrator::NavigationError;

#[derive(Debug)]
pub(crate) enum Error {
    NotFound(i64),
    Invalid(String),
    Environment(String),
    Incompatible(String),
    AlreadyFinished(i64),
    AlreadyAcknowledged(i64),
    StillRunning(i64),
    WrongStorage(i64),
    AgentNotFound(i64),
    InvalidAgentTransition {
        id: i64,
        from: AgentStatus,
        to: AgentStatus,
    },
    AgentNotDone(i64),
    AgentWrongStorage(i64),
    ReadOnly,
    CorruptData(String),
    Busy(String),
    Database(String),
    Navigation(NavigationError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "no process with id {id}"),
            Self::Invalid(message) | Self::Environment(message) => f.write_str(message),
            Self::Incompatible(message) => write!(f, "incompatible storage: {message}"),
            Self::AlreadyFinished(id) => write!(f, "process {id} is already finished"),
            Self::AlreadyAcknowledged(id) => write!(f, "process {id} is already acknowledged"),
            Self::StillRunning(id) => write!(f, "process {id} is still running"),
            Self::WrongStorage(id) => write!(f, "process {id} belongs to different storage"),
            Self::AgentNotFound(id) => write!(f, "no agent with id {id}"),
            Self::InvalidAgentTransition { id, from, to } => write!(
                f,
                "agent {id} cannot transition from {} to {}",
                from.as_str(),
                to.as_str()
            ),
            Self::AgentNotDone(id) => write!(f, "agent {id} is not done"),
            Self::AgentWrongStorage(id) => write!(f, "agent {id} belongs to different storage"),
            Self::ReadOnly => f.write_str("storage is read-only"),
            Self::CorruptData(message) => write!(f, "corrupt data: {message}"),
            Self::Busy(message) => write!(f, "storage unavailable: {message}"),
            Self::Database(message) => write!(f, "database error: {message}"),
            Self::Navigation(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<NavigationError> for Error {
    fn from(error: NavigationError) -> Self {
        Self::Navigation(error)
    }
}
