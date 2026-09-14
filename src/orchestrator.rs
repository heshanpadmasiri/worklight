//! Detection of a command's host and navigation back to it.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::Error;
use crate::storage::Storage;

#[allow(dead_code)]
pub(crate) struct Shell {
    id: i64,
    cwd: PathBuf,
    storage: Storage,
}

#[allow(dead_code)]
pub(crate) struct Tmux {
    id: i64,
    cwd: PathBuf,
    pane: String,
    storage: Storage,
}

pub(crate) enum Orchestrator {
    Shell(Shell),
    Tmux(Tmux),
}

impl Orchestrator {
    pub(crate) fn restore_shell(storage: Storage, id: i64, cwd: PathBuf) -> Result<Self, Error> {
        if id <= 0 {
            return Err(Error::CorruptData(format!("invalid shell id {id}")));
        }
        Ok(Self::Shell(Shell { id, cwd, storage }))
    }

    pub(crate) fn restore_tmux(
        storage: Storage,
        id: i64,
        cwd: PathBuf,
        pane: String,
    ) -> Result<Self, Error> {
        if id <= 0 || pane.is_empty() {
            return Err(Error::CorruptData(format!("invalid tmux destination {id}")));
        }
        Ok(Self::Tmux(Tmux {
            id,
            cwd,
            pane,
            storage,
        }))
    }

    #[allow(dead_code)]
    pub(crate) fn id(&self) -> i64 {
        match self {
            Self::Shell(value) => value.id,
            Self::Tmux(value) => value.id,
        }
    }
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Shell(_) => "shell",
            Self::Tmux(_) => "tmux",
        }
    }
    pub(crate) fn cwd(&self) -> &PathBuf {
        match self {
            Self::Shell(value) => &value.cwd,
            Self::Tmux(value) => &value.cwd,
        }
    }
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Shell(_) => "shell".into(),
            Self::Tmux(value) => format!("tmux {}", value.pane),
        }
    }
    pub(crate) fn navigate(&self, args: &[String]) -> Result<String, NavigationError> {
        match self {
            Self::Shell(value) => value.navigate(args),
            Self::Tmux(value) => value.navigate(args),
        }
    }
    pub(crate) fn snapshot(&self) -> OrchestratorSnapshot {
        match self {
            Self::Shell(value) => OrchestratorSnapshot::Shell {
                cwd: value.cwd.clone(),
            },
            Self::Tmux(value) => OrchestratorSnapshot::Tmux {
                cwd: value.cwd.clone(),
                pane: value.pane.clone(),
            },
        }
    }
    #[allow(dead_code)]
    pub(crate) fn belongs_to(&self, storage: &Storage) -> bool {
        match self {
            Self::Shell(value) => value.storage.same_storage(storage),
            Self::Tmux(value) => value.storage.same_storage(storage),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum OrchestratorSnapshot {
    Shell { cwd: PathBuf },
    Tmux { cwd: PathBuf, pane: String },
}

impl OrchestratorSnapshot {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Shell { .. } => "shell",
            Self::Tmux { .. } => "tmux",
        }
    }
    pub(crate) fn cwd(&self) -> &Path {
        match self {
            Self::Shell { cwd } | Self::Tmux { cwd, .. } => cwd,
        }
    }
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Shell { .. } => "shell".into(),
            Self::Tmux { pane, .. } => format!("tmux {pane}"),
        }
    }
}

#[derive(Debug)]
pub(crate) enum NavigationError {
    Unavailable(String),
    Invalid(String),
    Missing(String),
    Failed(String),
}
impl std::fmt::Display for NavigationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(v) | Self::Invalid(v) | Self::Missing(v) | Self::Failed(v) => {
                f.write_str(v)
            }
        }
    }
}
impl std::error::Error for NavigationError {}

pub(crate) trait Environment {
    fn var(&self, key: &str) -> Option<String>;
    fn cwd(&self) -> Result<PathBuf, String>;
}
pub(crate) struct SystemEnvironment;
impl Environment for SystemEnvironment {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string_lossy().into_owned())
    }
    fn cwd(&self) -> Result<PathBuf, String> {
        std::env::current_dir()
            .map_err(|e| format!("cannot determine the current working directory: {e}"))
    }
}

pub(crate) fn detect(env: &dyn Environment) -> Result<(PathBuf, Option<String>), String> {
    let cwd = env.cwd()?;
    Ok((cwd, detect_tmux(env)))
}
fn detect_tmux(env: &dyn Environment) -> Option<String> {
    env.var("TMUX").filter(|value| !value.is_empty())?;
    env.var("TMUX_PANE").filter(|value| !value.is_empty())
}

impl Shell {
    fn navigate(&self, _args: &[String]) -> Result<String, NavigationError> {
        Err(NavigationError::Unavailable(format!(
            "terminal navigation unavailable: run was started in a shell at {}",
            self.cwd.display()
        )))
    }
}
impl Tmux {
    fn navigate(&self, args: &[String]) -> Result<String, NavigationError> {
        let client = tmux_client(args)?;
        let listing = tmux_output(&[
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{session_name}:#{window_index}.#{pane_index}",
        ])
        .map_err(NavigationError::Failed)?;
        let target = listing
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .find(|(pane, _)| *pane == self.pane)
            .map(|(_, target)| target.to_string())
            .ok_or_else(|| {
                NavigationError::Missing(format!("tmux pane {} is no longer available", self.pane))
            })?;
        if let Some(client) = client {
            tmux_output(&["switch-client", "-c", client, "-t", &target])
                .map_err(NavigationError::Failed)?;
        } else {
            tmux_output(&["switch-client", "-t", &target]).map_err(NavigationError::Failed)?;
            tmux_output(&["select-pane", "-t", &self.pane]).map_err(NavigationError::Failed)?;
        }
        Ok(target)
    }
}
fn tmux_client(args: &[String]) -> Result<Option<&str>, NavigationError> {
    match args {
        [] => Ok(None),
        [option, client] if option == "--client" && !client.is_empty() => Ok(Some(client)),
        [option] if option.starts_with("--client=") && option.len() > 9 => Ok(Some(&option[9..])),
        [option] if option == "--client" || option == "--client=" => Err(NavigationError::Invalid(
            "tmux navigation option --client requires a value".into(),
        )),
        _ => Err(NavigationError::Invalid(format!(
            "unsupported tmux navigation arguments: {}",
            args.join(" ")
        ))),
    }
}
fn tmux_output(args: &[&str]) -> Result<String, String> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .map_err(|e| format!("could not run tmux: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
