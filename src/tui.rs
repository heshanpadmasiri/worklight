//! The Ratatui panel.
//!
//! Database-backed entities live on a worker thread. The terminal thread only
//! receives immutable snapshots and sends actions identified by process ID.

use std::collections::BTreeMap;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::{Frame, Terminal};

use crate::error::Error;
use crate::process::{ProcessRun, ProcessSnapshot};
use crate::storage::Storage;

const REFRESH: Duration = Duration::from_secs(5);
const REDRAW: Duration = Duration::from_millis(100);
const HISTORY_STEP: Duration = Duration::from_millis(25);
const DISCOVERY_BATCH: usize = 256;
const HISTORY_BATCH: usize = 128;

struct PanelData {
    storage: Storage,
    processes: BTreeMap<i64, ProcessRun>,
    discovered_through: i64,
    history_after: i64,
    history_through: Option<i64>,
    history_loaded: bool,
    show_history: bool,
    selected: Option<i64>,
    viewport_start: usize,
    viewport_len: usize,
    dry_run: bool,
    message: Option<String>,
}

struct PanelSnapshot {
    rows: Vec<ProcessSnapshot>,
    total_matching: usize,
    selected: Option<i64>,
    viewport_start: usize,
    history_loading: bool,
    show_history: bool,
    message: Option<String>,
}

struct Panel {
    snapshot: PanelSnapshot,
    state: TableState,
}

impl Panel {
    fn empty() -> Self {
        Self {
            snapshot: PanelSnapshot {
                rows: Vec::new(),
                total_matching: 0,
                selected: None,
                viewport_start: 0,
                history_loading: false,
                show_history: false,
                message: None,
            },
            state: TableState::default(),
        }
    }

    fn update(&mut self, snapshot: PanelSnapshot) {
        let selected = snapshot
            .selected
            .and_then(|id| snapshot.rows.iter().position(|process| process.id == id));
        self.snapshot = snapshot;
        self.state.select(selected);
    }

    fn selected(&self) -> Option<i64> {
        self.snapshot.selected
    }
}

enum PanelRequest {
    MoveSelection(isize),
    Resize(usize),
    ToggleHistory,
    Focus { id: i64, args: Vec<String> },
    Shutdown,
}

enum PanelEvent {
    Updated(PanelSnapshot),
    Focused,
    Failed(String),
}

pub(crate) fn run(path: &Path, dry_run: bool) -> Result<(), Error> {
    let (request_sender, request_receiver) = mpsc::channel();
    let (event_sender, event_receiver) = mpsc::channel();
    let worker_path = path.to_path_buf();
    let worker_handle = std::thread::spawn(move || {
        let result = worker(worker_path, dry_run, request_receiver, event_sender.clone());
        if let Err(error) = &result {
            let _ = event_sender.send(PanelEvent::Failed(error.to_string()));
        }
        result
    });

    let mut terminal = match enter() {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = request_sender.send(PanelRequest::Shutdown);
            let _ = worker_handle.join();
            return Err(error);
        }
    };

    let outcome = event_loop(&mut terminal, &request_sender, &event_receiver);
    let _ = request_sender.send(PanelRequest::Shutdown);
    let restored = leave(&mut terminal);
    let worker_outcome = worker_handle
        .join()
        .map_err(|_| Error::Invalid("panel worker panicked".into()))?;

    outcome.and(restored).and(worker_outcome)
}

fn worker(
    path: PathBuf,
    dry_run: bool,
    requests: Receiver<PanelRequest>,
    events: Sender<PanelEvent>,
) -> Result<(), Error> {
    let storage = if dry_run {
        Storage::open_read_only(&path)?
    } else {
        Storage::open(&path)?
    };
    let mut data = PanelData::load(storage, dry_run)?;
    if events.send(PanelEvent::Updated(data.snapshot())).is_err() {
        return Ok(());
    }

    let mut next_refresh = Instant::now() + REFRESH;
    let mut next_history = Instant::now();
    let mut history_retry = Instant::now();

    loop {
        let now = Instant::now();
        let history_ready = data.show_history
            && !data.history_loaded
            && now >= next_history
            && now >= history_retry;
        let wait = if history_ready {
            Duration::ZERO
        } else {
            next_refresh.saturating_duration_since(now).min(REDRAW)
        };

        match requests.recv_timeout(wait) {
            Ok(PanelRequest::Shutdown) => return Ok(()),
            Ok(request) => {
                let result = match request {
                    PanelRequest::MoveSelection(delta) => {
                        data.move_selection(delta);
                        Ok(false)
                    }
                    PanelRequest::Resize(rows) => {
                        data.resize(rows);
                        Ok(false)
                    }
                    PanelRequest::ToggleHistory => data.toggle_history().map(|()| false),
                    PanelRequest::Focus { id, args } => data.focus(id, &args),
                    PanelRequest::Shutdown => unreachable!(),
                };
                match result {
                    Ok(focused) => {
                        if events.send(PanelEvent::Updated(data.snapshot())).is_err() {
                            return Ok(());
                        }
                        if focused && events.send(PanelEvent::Focused).is_err() {
                            return Ok(());
                        }
                    }
                    Err(error) => {
                        let message = error.to_string();
                        data.message = Some(message.clone());
                        if events.send(PanelEvent::Failed(message)).is_err() {
                            return Ok(());
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        let now = Instant::now();
        if now >= next_refresh {
            next_refresh = now + REFRESH;
            match data.sync() {
                Ok(()) => {
                    data.message = None;
                    if events.send(PanelEvent::Updated(data.snapshot())).is_err() {
                        return Ok(());
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    data.message = Some(message.clone());
                    if events.send(PanelEvent::Failed(message)).is_err() {
                        return Ok(());
                    }
                }
            }
        }

        let now = Instant::now();
        if data.show_history && !data.history_loaded && now >= next_history && now >= history_retry
        {
            match data.load_history_batch() {
                Ok(()) => {
                    data.message = None;
                    history_retry = now;
                    if events.send(PanelEvent::Updated(data.snapshot())).is_err() {
                        return Ok(());
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    data.message = Some(message.clone());
                    history_retry = now + REFRESH;
                    if events.send(PanelEvent::Failed(message)).is_err() {
                        return Ok(());
                    }
                }
            }
            next_history = now + HISTORY_STEP;
        }
    }
}

impl PanelData {
    fn load(storage: Storage, dry_run: bool) -> Result<Self, Error> {
        // Capture the watermark before listing. A process created in between is
        // harmlessly merged again by discovery, while no newer ID is skipped.
        let discovered_through = storage.latest_process()?.unwrap_or(0);
        let initial = storage.unacked_process()?;
        let mut panel = Self {
            storage,
            processes: BTreeMap::new(),
            discovered_through,
            history_after: 0,
            history_through: None,
            history_loaded: false,
            show_history: false,
            selected: None,
            viewport_start: 0,
            viewport_len: 0,
            dry_run,
            message: None,
        };
        panel.merge(initial)?;
        panel.normalize_selection();
        Ok(panel)
    }

    fn sync(&mut self) -> Result<(), Error> {
        let through = self.storage.latest_process()?.unwrap_or(0);
        while self.discovered_through < through {
            let after = self.discovered_through;
            let found = self
                .storage
                .range_process(after, through, DISCOVERY_BATCH)?;
            let loaded_through = found.last().map(ProcessRun::id).unwrap_or(through);
            self.merge(found)?;
            self.discovered_through = loaded_through;
        }

        let mut pending: Vec<&mut ProcessRun> = self
            .processes
            .values_mut()
            .filter(|process| process.needs_sync())
            .collect();
        self.storage.sync_process(&mut pending)?;
        self.normalize_selection();
        Ok(())
    }

    fn load_history_batch(&mut self) -> Result<(), Error> {
        if self.history_loaded || !self.show_history {
            return Ok(());
        }
        let through = self.history_through.unwrap_or(0);
        if self.history_after >= through {
            self.history_loaded = true;
            self.normalize_selection();
            return Ok(());
        }

        let after = self.history_after;
        let found = self.storage.range_process(after, through, HISTORY_BATCH)?;
        let loaded_through = found.last().map(ProcessRun::id).unwrap_or(through);
        self.merge(found)?;
        self.history_after = loaded_through;
        if self.history_after >= through {
            self.history_loaded = true;
        }
        self.normalize_selection();
        Ok(())
    }

    fn toggle_history(&mut self) -> Result<(), Error> {
        if !self.show_history && self.history_through.is_none() {
            let through = self.storage.latest_process()?.unwrap_or(0);
            self.history_through = Some(through);
            self.history_loaded = through == 0;
        }
        self.show_history = !self.show_history;
        self.message = None;
        self.normalize_selection();
        Ok(())
    }

    fn merge(&mut self, processes: Vec<ProcessRun>) -> Result<(), Error> {
        for process in processes {
            let id = process.id();
            if let Some(existing) = self.processes.get_mut(&id) {
                existing.observe(process.snapshot().state)?;
            } else {
                self.processes.insert(id, process);
            }
        }
        self.normalize_selection();
        Ok(())
    }

    fn move_selection(&mut self, delta: isize) {
        let ids = self.matching_ids();
        if ids.is_empty() {
            self.selected = None;
            self.viewport_start = 0;
            return;
        }
        let current = self
            .selected
            .and_then(|id| ids.iter().position(|candidate| *candidate == id))
            .unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, ids.len() as isize - 1) as usize;
        self.selected = Some(ids[next]);
        self.ensure_selected_visible(&ids);
    }

    fn resize(&mut self, rows: usize) {
        self.viewport_len = rows;
        self.normalize_selection();
    }

    fn focus(&mut self, id: i64, args: &[String]) -> Result<bool, Error> {
        let process = self.processes.get_mut(&id).ok_or(Error::NotFound(id))?;
        if self.dry_run {
            validate_navigation_args(process.orchestrator().kind(), args)?;
            process.state()?;
            let destination = format!(
                "{} ({})",
                process.orchestrator().describe(),
                process.orchestrator().cwd().display()
            );
            self.message = Some(format!("dry run: would navigate to {destination}"));
            self.normalize_selection();
            return Ok(false);
        }

        process.focus(args)?;
        self.message = None;
        self.normalize_selection();
        Ok(true)
    }

    fn snapshot(&self) -> PanelSnapshot {
        let ids = self.matching_ids();
        let start = self.viewport_start.min(ids.len());
        let end = start.saturating_add(self.viewport_len).min(ids.len());
        let rows = ids[start..end]
            .iter()
            .filter_map(|id| self.processes.get(id))
            .map(ProcessRun::snapshot)
            .collect();
        PanelSnapshot {
            rows,
            total_matching: ids.len(),
            selected: self.selected,
            viewport_start: start,
            history_loading: self.show_history && !self.history_loaded,
            show_history: self.show_history,
            message: self.message.clone(),
        }
    }

    fn matching_ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .processes
            .iter()
            .filter(|(_, process)| self.show_history || process.needs_sync())
            .map(|(id, _)| *id)
            .collect();
        ids.sort_by(|left, right| {
            let left_process = self.processes.get(left).expect("ID came from map");
            let right_process = self.processes.get(right).expect("ID came from map");
            right_process
                .started_at()
                .cmp(&left_process.started_at())
                .then_with(|| right.cmp(left))
        });
        ids
    }

    fn normalize_selection(&mut self) {
        let ids = self.matching_ids();
        if ids.is_empty() {
            self.selected = None;
            self.viewport_start = 0;
            return;
        }

        if !self
            .selected
            .is_some_and(|selected| ids.contains(&selected))
        {
            self.selected = self
                .selected
                .and_then(|selected| self.nearest_visible(selected, &ids))
                .or_else(|| ids.first().copied());
        }
        self.ensure_selected_visible(&ids);
    }

    fn nearest_visible(&self, selected: i64, visible: &[i64]) -> Option<i64> {
        let mut all: Vec<i64> = self.processes.keys().copied().collect();
        all.sort_by(|left, right| {
            let left_process = self.processes.get(left).expect("ID came from map");
            let right_process = self.processes.get(right).expect("ID came from map");
            right_process
                .started_at()
                .cmp(&left_process.started_at())
                .then_with(|| right.cmp(left))
        });
        let old = all.iter().position(|id| *id == selected)?;
        visible
            .iter()
            .min_by_key(|id| {
                all.iter()
                    .position(|candidate| candidate == *id)
                    .map_or(usize::MAX, |position| position.abs_diff(old))
            })
            .copied()
    }

    fn ensure_selected_visible(&mut self, ids: &[i64]) {
        let Some(selected) = self.selected else {
            self.viewport_start = 0;
            return;
        };
        let Some(index) = ids.iter().position(|id| *id == selected) else {
            return;
        };
        if self.viewport_len == 0 {
            self.viewport_start = index;
            return;
        }
        if index < self.viewport_start {
            self.viewport_start = index;
        } else if index >= self.viewport_start.saturating_add(self.viewport_len) {
            self.viewport_start = index + 1 - self.viewport_len;
        }
        self.viewport_start = self
            .viewport_start
            .min(ids.len().saturating_sub(self.viewport_len));
    }
}

fn validate_navigation_args(kind: &str, args: &[String]) -> Result<(), Error> {
    if kind != "tmux" {
        return Ok(());
    }
    match args {
        [] => Ok(()),
        [option, client] if option == "--client" && !client.is_empty() => Ok(()),
        [option] if option.starts_with("--client=") && option.len() > 9 => Ok(()),
        _ => Err(Error::Invalid(format!(
            "unsupported tmux navigation arguments: {}",
            args.join(" ")
        ))),
    }
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    requests: &Sender<PanelRequest>,
    events: &Receiver<PanelEvent>,
) -> Result<(), Error> {
    let mut panel = Panel::empty();
    let size = terminal.size().map_err(terminal_error)?;
    send_request(requests, PanelRequest::Resize(viewport_rows(size.height)))?;

    loop {
        if consume_events(events, &mut panel)? {
            return Ok(());
        }
        terminal
            .draw(|frame| draw(frame, &mut panel))
            .map_err(terminal_error)?;

        if event::poll(REDRAW).map_err(terminal_error)? {
            match event::read().map_err(terminal_error)? {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => {
                        send_request(requests, PanelRequest::MoveSelection(1))?;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        send_request(requests, PanelRequest::MoveSelection(-1))?;
                    }
                    KeyCode::Char('h') => {
                        send_request(requests, PanelRequest::ToggleHistory)?;
                    }
                    KeyCode::Enter => navigate(&panel, requests)?,
                    _ => {}
                },
                Event::Resize(_, height) => {
                    send_request(requests, PanelRequest::Resize(viewport_rows(height)))?;
                }
                _ => {}
            }
        }
    }
}

/// Consume all currently queued events, retaining only the newest display.
fn consume_events(events: &Receiver<PanelEvent>, panel: &mut Panel) -> Result<bool, Error> {
    let mut latest = None;
    let mut focused = false;
    loop {
        match events.try_recv() {
            Ok(PanelEvent::Updated(snapshot)) => latest = Some(snapshot),
            Ok(PanelEvent::Failed(message)) => {
                if let Some(snapshot) = latest.as_mut() {
                    snapshot.message = Some(message);
                } else {
                    panel.snapshot.message = Some(message);
                }
            }
            Ok(PanelEvent::Focused) => focused = true,
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                // Let `run` join the worker and return its structured startup or
                // runtime error (or report a panic) instead of replacing it
                // with a generic channel error.
                return Ok(true);
            }
        }
    }
    if let Some(snapshot) = latest {
        panel.update(snapshot);
    }
    Ok(focused)
}

fn send_request(requests: &Sender<PanelRequest>, request: PanelRequest) -> Result<(), Error> {
    requests
        .send(request)
        .map_err(|_| Error::Invalid("panel worker stopped".into()))
}

fn viewport_rows(height: u16) -> usize {
    // Two footer rows, two table borders, and one table header.
    usize::from(height.saturating_sub(5))
}

fn navigate(panel: &Panel, requests: &Sender<PanelRequest>) -> Result<(), Error> {
    let Some(id) = panel.selected() else {
        return Ok(());
    };
    let args = std::env::var("WORKLIGHT_TMUX_CLIENT")
        .map(|client| vec!["--client".to_string(), client])
        .unwrap_or_default();
    send_request(requests, PanelRequest::Focus { id, args })
}

fn enter() -> Result<Terminal<CrosstermBackend<Stdout>>, Error> {
    enable_raw_mode().map_err(terminal_error)?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        return Err(rollback_enter(error));
    }
    match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(terminal) => Ok(terminal),
        Err(error) => Err(rollback_enter(error)),
    }
}

/// Raw mode is already active when alternate-screen setup or terminal
/// construction fails. Best-effort rollback is safe at this initialization
/// boundary, but cleanup failures are included rather than hidden.
fn rollback_enter(error: io::Error) -> Error {
    let mut message = format!("terminal error: {error}");
    if let Err(cleanup) = execute!(io::stdout(), LeaveAlternateScreen) {
        message.push_str(&format!("; could not leave alternate screen: {cleanup}"));
    }
    if let Err(cleanup) = disable_raw_mode() {
        message.push_str(&format!("; could not disable raw mode: {cleanup}"));
    }
    Error::Invalid(message)
}

fn leave(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<(), Error> {
    let raw = disable_raw_mode();
    let screen = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let cursor = terminal.show_cursor();
    raw.and(screen).and(cursor).map_err(terminal_error)
}

fn terminal_error(error: io::Error) -> Error {
    Error::Invalid(format!("terminal error: {error}"))
}

fn draw(frame: &mut Frame, panel: &mut Panel) {
    let areas = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let now = SystemTime::now();
    let rows: Vec<Row> = panel
        .snapshot
        .rows
        .iter()
        .map(|process| {
            Row::new(vec![
                Cell::from(process.label.clone()),
                Cell::from(process.outcome()),
                Cell::from(
                    process
                        .exit_status()
                        .map_or_else(|| "-".to_string(), |code| code.to_string()),
                ),
                Cell::from(format_elapsed(process.elapsed(now))),
                Cell::from(process.orchestrator.describe()),
                Cell::from(process.orchestrator.cwd().display().to_string()),
                Cell::from(if process.acknowledged() { "yes" } else { "no" }),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Min(20),
            Constraint::Length(12),
            Constraint::Length(4),
            Constraint::Length(9),
            Constraint::Length(24),
            Constraint::Min(16),
            Constraint::Length(4),
        ],
    )
    .header(
        Row::new(vec![
            "command", "state", "exit", "elapsed", "where", "cwd", "ack",
        ])
        .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .block(Block::default().borders(Borders::ALL).title("worklight"));

    frame.render_stateful_widget(table, areas[0], &mut panel.state);

    let mode = if panel.snapshot.show_history {
        "all"
    } else {
        "unacked"
    };
    let loading = if panel.snapshot.history_loading {
        " (loading history)"
    } else {
        ""
    };
    let range = if panel.snapshot.rows.is_empty() {
        "0".to_string()
    } else {
        let first = panel.snapshot.viewport_start + 1;
        let last = panel.snapshot.viewport_start + panel.snapshot.rows.len();
        format!("{first}-{last}")
    };
    let status = match &panel.snapshot.message {
        Some(message) => Paragraph::new(format!("error: {message}"))
            .style(Style::default().add_modifier(Modifier::BOLD)),
        None => Paragraph::new(format!(
            "{} tracked ({mode}){loading}, showing {range}",
            panel.snapshot.total_matching
        )),
    };
    frame.render_widget(status, areas[1]);
    frame.render_widget(
        Paragraph::new("j/k or arrows select  enter navigate  h history  q quit"),
        areas[2],
    );
}

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}
