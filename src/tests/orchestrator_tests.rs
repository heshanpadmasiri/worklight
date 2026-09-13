use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::orchestrator::{self, Environment, NavigationError};

struct FakeEnvironment {
    cwd: PathBuf,
    vars: HashMap<String, String>,
}
impl FakeEnvironment {
    fn new(cwd: &str) -> Self {
        Self {
            cwd: cwd.into(),
            vars: HashMap::new(),
        }
    }
    fn with(mut self, key: &str, value: &str) -> Self {
        self.vars.insert(key.into(), value.into());
        self
    }
}
impl Environment for FakeEnvironment {
    fn var(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }
    fn cwd(&self) -> Result<PathBuf, String> {
        Ok(self.cwd.clone())
    }
}

#[test]
fn detection_returns_creation_data_without_fabricated_identity() {
    let shell = FakeEnvironment::new("/work");
    assert_eq!(
        orchestrator::detect(&shell).unwrap(),
        (PathBuf::from("/work"), None)
    );
    let tmux = FakeEnvironment::new("/work")
        .with("TMUX", "/tmp/socket,123,0")
        .with("TMUX_PANE", "%7");
    assert_eq!(
        orchestrator::detect(&tmux).unwrap(),
        (PathBuf::from("/work"), Some("%7".into()))
    );
}

#[test]
fn tmux_requires_both_active_environment_and_nonempty_pane() {
    let no_server = FakeEnvironment::new("/work").with("TMUX_PANE", "%1");
    let no_pane = FakeEnvironment::new("/work").with("TMUX", "anything");
    assert_eq!(orchestrator::detect(&no_server).unwrap().1, None);
    assert_eq!(orchestrator::detect(&no_pane).unwrap().1, None);
}

#[test]
fn shell_navigation_remains_unsupported() {
    let fixture = super::support::Fixture::new();
    let process = fixture
        .storage()
        .create_process("shell", Path::new("/work"), None)
        .unwrap();
    let error = process.orchestrator().navigate(&[]).unwrap_err();
    assert!(matches!(error, NavigationError::Unavailable(_)));
}
