//! Config Syntax Checker screen — validates JSON/TOML/YAML/XML files,
//! local or on a remote host over SSH (picked on the Target tab, same
//! Local/Remote shape as Kernel Tuner). Errors are shown in a large
//! scrollable overlay rather than the small History panel, since a
//! parser's own error text is often several lines and needs to actually be
//! readable. From that overlay, an invalid file can be sent through a
//! best-effort automatic fixer (`config_check::fixer`) — only after an
//! explicit, keyboard-only confirmation, since that step writes to the
//! file (a backup is kept either way).

use std::{sync::mpsc, thread};

use arboard::Clipboard;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::config_check::{
    engine::{self, Entry},
    exec::{ExecSession, Target},
    fixer,
    format::{self, Format},
};

use super::file_picker::FilePicker;
use super::host_picker::HostPicker;
use super::mouse;
use super::widgets::*;
use super::{file_picker, host_picker};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Target,
    Check,
}

// ── Target tab (same shape as kerneltune_screen::TargetTab) ────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetField {
    Mode,
    SshHost,
    SshPort,
    SshUser,
    SshKeyPath,
    SshPassword,
    BtnConnect,
}

fn target_active_fields(is_remote: bool) -> Vec<TargetField> {
    let mut v = vec![TargetField::Mode];
    if is_remote {
        v.extend([TargetField::SshHost, TargetField::SshPort, TargetField::SshUser, TargetField::SshKeyPath, TargetField::SshPassword]);
    }
    v.push(TargetField::BtnConnect);
    v
}

fn target_form_rows(form_inner: Rect, is_remote: bool) -> Vec<Rect> {
    let mut constraints = vec![
        Constraint::Length(1), // [0] Mode
        Constraint::Length(1), // [1] spacer
    ];
    if is_remote {
        constraints.extend([
            Constraint::Length(1), // [2] SSH Host
            Constraint::Length(1), // [3] spacer
            Constraint::Length(1), // [4] SSH Port
            Constraint::Length(1), // [5] spacer
            Constraint::Length(1), // [6] SSH User
            Constraint::Length(1), // [7] spacer
            Constraint::Length(1), // [8] SSH Key Path
            Constraint::Length(1), // [9] spacer
            Constraint::Length(1), // [10] SSH Password
            Constraint::Length(1), // [11] spacer
        ]);
    }
    constraints.extend([
        Constraint::Length(1), // Connect button
        Constraint::Length(1), // spacer
        Constraint::Length(1), // nav hint
        Constraint::Min(0),
    ]);
    Layout::default().direction(Direction::Vertical).margin(1).constraints(constraints).split(form_inner).to_vec()
}

struct TargetTab {
    is_remote: bool,
    ssh_host: Input,
    ssh_port: Input,
    ssh_user: Input,
    ssh_key_path: Input,
    ssh_password: Input,
    field: TargetField,
    host_picker: Option<HostPicker>,
    key_picker: Option<FilePicker>,
    connecting: bool,
    connected_label: Option<String>,
}

impl TargetTab {
    fn new() -> Self {
        Self {
            is_remote: false,
            ssh_host: Input::default(),
            ssh_port: Input::new("22"),
            ssh_user: Input::default(),
            ssh_key_path: Input::default(),
            ssh_password: Input::default(),
            field: TargetField::Mode,
            host_picker: None,
            key_picker: None,
            connecting: false,
            connected_label: None,
        }
    }

    fn next_field(&mut self) {
        let fields = target_active_fields(self.is_remote);
        let idx = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = fields[(idx + 1) % fields.len()];
    }

    fn prev_field(&mut self) {
        let fields = target_active_fields(self.is_remote);
        let idx = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = fields[(idx + fields.len() - 1) % fields.len()];
    }

    fn fill_from_host(&mut self, server: &crate::easyssh_mgr::config::Server) {
        self.ssh_host = Input::new(server.effective_host());
        if server.port != 0 {
            self.ssh_port = Input::new(&server.port.to_string());
        }
        if !server.user.is_empty() {
            self.ssh_user = Input::new(&server.user);
        }
        if let Some(key) = server.identity_files.first() {
            self.ssh_key_path = Input::new(key);
        }
        if !server.ssh_password.is_empty() {
            self.ssh_password = Input::new(&server.ssh_password);
        }
    }

    fn as_target(&self) -> Target {
        if self.is_remote {
            Target::Remote {
                host: self.ssh_host.value().trim().to_string(),
                port: self.ssh_port.value().trim().to_string(),
                user: self.ssh_user.value().trim().to_string(),
                key_path: self.ssh_key_path.value().trim().to_string(),
                password: self.ssh_password.value().to_string(),
            }
        } else {
            Target::Local
        }
    }
}

// ── Check tab ────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum CheckField {
    Path,
    Format,
    BtnBrowse,
    BtnCheck,
}

const CHECK_FIELDS: [CheckField; 4] = [CheckField::Path, CheckField::Format, CheckField::BtnBrowse, CheckField::BtnCheck];

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormatChoice {
    Auto,
    Forced(Format),
}

impl FormatChoice {
    fn label(self) -> &'static str {
        match self {
            FormatChoice::Auto => "Auto",
            FormatChoice::Forced(f) => f.label(),
        }
    }

    fn cycle(self, delta: i32) -> Self {
        // Auto, then each Format in turn.
        let all_len = Format::ALL.len() as i32;
        let cur = match self {
            FormatChoice::Auto => -1,
            FormatChoice::Forced(f) => Format::ALL.iter().position(|x| *x == f).unwrap_or(0) as i32,
        };
        let new = (cur + delta + 1).rem_euclid(all_len + 1) - 1;
        if new < 0 {
            FormatChoice::Auto
        } else {
            FormatChoice::Forced(Format::ALL[new as usize])
        }
    }

    fn resolve(self, path: &str) -> Option<Format> {
        match self {
            FormatChoice::Auto => format::detect(path),
            FormatChoice::Forced(f) => Some(f),
        }
    }
}

/// Which config-file browser row was activated on `Enter`/click.
enum BrowserAction {
    None,
    Navigate(String),
    Picked(String),
}

/// Browses a directory over `config_check::exec::ExecSession` — the exact
/// same command (`ls -1Ap`) works for `Target::Local` and `Target::Remote`,
/// so this one type serves both, unlike the local-filesystem-only
/// `file_picker::FilePicker` used elsewhere in the app for picking an SSH
/// key file. Each navigation step is a fresh SSH round trip (see
/// `ConfigCheckScreen::start_browse`), so this shows a `loading` state
/// rather than blocking the render loop.
struct Browser {
    cwd: String,
    entries: Vec<Entry>,
    selected: usize,
    error: Option<String>,
    loading: bool,
}

impl Browser {
    fn new(start: &str) -> Self {
        Self { cwd: start.to_string(), entries: Vec::new(), selected: 0, error: None, loading: true }
    }

    fn up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn down(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    fn on_enter(&self) -> BrowserAction {
        let Some(entry) = self.entries.get(self.selected) else { return BrowserAction::None };
        if entry.name == ".." {
            BrowserAction::Navigate(engine::parent_dir(&self.cwd))
        } else if entry.is_dir {
            BrowserAction::Navigate(engine::join_dir(&self.cwd, &entry.name))
        } else {
            BrowserAction::Picked(engine::join_dir(&self.cwd, &entry.name))
        }
    }

    /// Applies a listing result, dropping it if it's a stale reply for a
    /// directory the user has already navigated away from (possible if
    /// they hit Enter/`..` again before the previous round trip returned).
    fn apply_result(&mut self, cwd: String, result: Result<Vec<Entry>, String>) {
        if cwd != self.cwd {
            return;
        }
        self.loading = false;
        match result {
            Ok(mut listed) => {
                listed.sort_by(|a, b| (!a.is_dir, a.name.to_lowercase()).cmp(&(!b.is_dir, b.name.to_lowercase())));
                let mut entries = Vec::new();
                if self.cwd != "/" {
                    entries.push(Entry { name: "..".to_string(), is_dir: true });
                }
                entries.extend(listed);
                self.entries = entries;
                self.selected = 0;
                self.error = None;
            }
            Err(e) => {
                self.entries.clear();
                self.error = Some(e);
            }
        }
    }

    /// Mirrors `file_picker::FilePicker::row_at` — same modal size/layout.
    fn row_at(&self, area: Rect, x: u16, y: u16) -> Option<usize> {
        let width = 84u16.min(area.width.saturating_sub(4));
        let height = 24u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);
        let inner = Rect { x: modal_area.x + 1, y: modal_area.y + 1, width: modal_area.width.saturating_sub(2), height: modal_area.height.saturating_sub(2) };
        let rows = Layout::default().direction(Direction::Vertical).margin(1).constraints([Constraint::Min(3), Constraint::Length(1)]).split(inner);
        let list_area = rows[0];
        if x <= list_area.x || x + 1 >= list_area.x + list_area.width {
            return None;
        }
        if y < list_area.y || y >= list_area.y + list_area.height {
            return None;
        }
        let idx = (y - list_area.y) as usize;
        if idx < self.entries.len() {
            Some(idx)
        } else {
            None
        }
    }
}

fn draw_browser(f: &mut Frame, b: &Browser, area: Rect) {
    let width = 84u16.min(area.width.saturating_sub(4));
    let height = 24u16.min(area.height.saturating_sub(2));
    let modal_area = centered_rect(width, height, area);

    f.render_widget(Clear, modal_area);
    let block = Block::default()
        .title(Span::styled(format!(" Select Config File — {} ", b.cwd), Style::default().fg(title_color())))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent()))
        .style(Style::default().bg(bg2()));
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    let rows = Layout::default().direction(Direction::Vertical).margin(1).constraints([Constraint::Min(3), Constraint::Length(1)]).split(inner);

    if b.loading {
        f.render_widget(Paragraph::new(Line::from(Span::styled("Loading…", Style::default().fg(yellow())))), rows[0]);
    } else if let Some(err) = &b.error {
        f.render_widget(Paragraph::new(Line::from(Span::styled(format!("can't list this directory: {err}"), Style::default().fg(red())))), rows[0]);
    } else {
        let items: Vec<ListItem> = b
            .entries
            .iter()
            .map(|e| {
                let label = if e.is_dir { format!("{}/", e.name) } else { e.name.clone() };
                let style = if e.is_dir { Style::default().fg(accent()).add_modifier(Modifier::BOLD) } else { Style::default().fg(fg()) };
                ListItem::new(Span::styled(label, style))
            })
            .collect();
        let list = List::new(items).highlight_style(focused()).style(Style::default().fg(fg()).bg(bg2()));
        let mut state = ListState::default();
        if !b.entries.is_empty() {
            state.select(Some(b.selected.min(b.entries.len() - 1)));
        }
        f.render_stateful_widget(list, rows[0], &mut state);
    }

    f.render_widget(
        Paragraph::new(Line::from(Span::styled("\u{2191}\u{2193} navigate  Enter open dir / pick file  Esc cancel", lbl()))),
        rows[1],
    );
}

struct CheckResult {
    path: String,
    format: Format,
    valid: bool,
    error_text: String,
    fix_backup: Option<String>,
    fix_message: Option<String>,
}

struct CheckTab {
    path: Input,
    format_choice: FormatChoice,
    field: CheckField,
    browser: Option<Browser>,
    checking: bool,
    result: Option<CheckResult>,
    show_results: bool,
    results_scroll: u16,
    confirm_fix: bool,
    fixing: bool,
}

impl CheckTab {
    fn new() -> Self {
        Self {
            path: Input::default(),
            format_choice: FormatChoice::Auto,
            field: CheckField::Path,
            browser: None,
            checking: false,
            result: None,
            show_results: false,
            results_scroll: 0,
            confirm_fix: false,
            fixing: false,
        }
    }

    fn next_field(&mut self) {
        let idx = CHECK_FIELDS.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = CHECK_FIELDS[(idx + 1) % CHECK_FIELDS.len()];
    }

    fn prev_field(&mut self) {
        let idx = CHECK_FIELDS.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = CHECK_FIELDS[(idx + CHECK_FIELDS.len() - 1) % CHECK_FIELDS.len()];
    }
}

fn check_form_rows(form_inner: Rect) -> Vec<Rect> {
    Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // [0] Path
            Constraint::Length(1), // [1] spacer
            Constraint::Length(1), // [2] Format
            Constraint::Length(1), // [3] spacer
            Constraint::Length(1), // [4] Browse / Check buttons
            Constraint::Length(1), // [5] spacer
            Constraint::Length(1), // [6] nav hint
            Constraint::Min(0),
        ])
        .split(form_inner)
        .to_vec()
}

/// Where the config-file browser should start: the directory the current
/// Path field points at (its parent, if the field already names a
/// recognizable config file), or a sane per-mode default when it's empty.
/// Remote paths are used as-is, never through `crate::config::expand_path`
/// — that resolves `~` against *this* machine's home directory, which is
/// meaningless (and silently wrong) for a path that's actually going to be
/// read on the other end of an SSH connection.
fn starting_dir(path_field: &str, is_remote: bool) -> String {
    let raw = path_field.trim();
    if raw.is_empty() {
        return if is_remote { "/etc".to_string() } else { dirs::home_dir().map(|p| p.display().to_string()).unwrap_or_else(|| "/".to_string()) };
    }
    let resolved = if is_remote { raw.to_string() } else { crate::config::expand_path(raw) };
    if format::detect(&resolved).is_some() {
        engine::parent_dir(&resolved)
    } else {
        resolved
    }
}

// ── Screen ───────────────────────────────────────────────────────────────
enum Msg {
    Connected(String),
    ConnectFailed(String),
    BrowseResult { cwd: String, result: Result<Vec<Entry>, String> },
    CheckedFile { path: String, format: Format, result: Result<String, String> },
    FixDone { result: Result<FixOutcome, String> },
}

struct FixOutcome {
    backup: String,
    now_valid: bool,
    remaining_error: Option<String>,
}

pub struct ConfigCheckScreen {
    tab: Tab,
    target_tab: TargetTab,
    check_tab: CheckTab,
    history: Vec<(bool, String)>,
    history_scroll: u16,
    modal: Option<(String, String)>,

    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
}

impl ConfigCheckScreen {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tab: Tab::Target,
            target_tab: TargetTab::new(),
            check_tab: CheckTab::new(),
            history: Vec::new(),
            history_scroll: 0,
            modal: None,
            tx,
            rx,
        }
    }

    pub fn tick(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Connected(label) => {
                    self.target_tab.connecting = false;
                    self.target_tab.connected_label = Some(label.clone());
                    self.history.push((true, format!("Connected to {label}")));
                }
                Msg::ConnectFailed(e) => {
                    self.target_tab.connecting = false;
                    self.target_tab.connected_label = None;
                    self.history.push((false, format!("Connect failed: {}", one_line(&e))));
                }
                Msg::BrowseResult { cwd, result } => {
                    if let Some(b) = &mut self.check_tab.browser {
                        b.apply_result(cwd, result);
                    }
                }
                Msg::CheckedFile { path, format, result } => {
                    self.check_tab.checking = false;
                    match result {
                        Ok(content) => {
                            let validation = format::validate(format, &content);
                            let valid = validation.is_ok();
                            let error_text = validation.err().unwrap_or_default();
                            self.history.push((
                                valid,
                                if valid { format!("{}: valid {}", path, format.label()) } else { format!("{}: invalid {} — see the results window", path, format.label()) },
                            ));
                            self.check_tab.result = Some(CheckResult { path, format, valid, error_text, fix_backup: None, fix_message: None });
                            self.check_tab.show_results = true;
                            self.check_tab.results_scroll = 0;
                        }
                        Err(e) => {
                            self.history.push((false, format!("couldn't read {path}: {}", one_line(&e))));
                            self.modal = Some(("Error".to_string(), e));
                        }
                    }
                }
                Msg::FixDone { result } => {
                    self.check_tab.fixing = false;
                    match result {
                        Ok(outcome) => {
                            if let Some(r) = &mut self.check_tab.result {
                                r.valid = outcome.now_valid;
                                r.error_text = outcome.remaining_error.clone().unwrap_or_default();
                                r.fix_backup = Some(outcome.backup.clone());
                                r.fix_message = Some(if outcome.now_valid {
                                    "Auto-fix applied — the file now parses cleanly.".to_string()
                                } else {
                                    "Auto-fix applied, but some errors remain — see below.".to_string()
                                });
                            }
                            self.check_tab.results_scroll = 0;
                            self.history.push((outcome.now_valid, format!("auto-fix written, backup at {}", outcome.backup)));
                        }
                        Err(e) => {
                            self.history.push((false, format!("auto-fix failed: {}", one_line(&e))));
                            self.modal = Some(("Auto-Fix Failed".to_string(), e));
                        }
                    }
                }
            }
        }
    }

    fn error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.history.push((false, msg.clone()));
        self.modal = Some(("Error".to_string(), msg));
    }

    // ── Actions ──────────────────────────────────────────────────────
    fn trigger_connect(&mut self) {
        let target = self.target_tab.as_target();
        if let Target::Remote { host, .. } = &target {
            if host.trim().is_empty() {
                self.error("Enter a remote host first (or switch Mode to Local)");
                return;
            }
        }
        self.target_tab.connecting = true;
        let label = target.label();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = (|| -> Result<(), String> {
                let session = ExecSession::open(&target)?;
                session.run_checked("echo ok").map(|_| ())
            })();
            match result {
                Ok(()) => {
                    let _ = tx.send(Msg::Connected(label));
                }
                Err(e) => {
                    let _ = tx.send(Msg::ConnectFailed(e));
                }
            }
        });
    }

    fn start_browse(&mut self) {
        let is_remote = self.target_tab.is_remote;
        if is_remote && self.target_tab.ssh_host.value().trim().is_empty() {
            self.error("Enter a remote host on the Target tab first");
            return;
        }
        let start = starting_dir(self.check_tab.path.value(), is_remote);
        if is_remote && !start.starts_with('/') {
            self.error("Remote paths must be absolute (e.g. /etc/nginx/nginx.conf) — \"~\" can't be resolved without running a command first");
            return;
        }
        self.check_tab.browser = Some(Browser::new(&start));
        self.list_dir(start);
    }

    fn list_dir(&mut self, path: String) {
        let target = self.target_tab.as_target();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = (|| -> Result<Vec<Entry>, String> {
                let session = ExecSession::open(&target)?;
                engine::list_dir(&session, &path)
            })();
            let _ = tx.send(Msg::BrowseResult { cwd: path, result });
        });
    }

    fn trigger_check(&mut self) {
        let raw_path = self.check_tab.path.value().trim().to_string();
        if raw_path.is_empty() {
            self.error("Enter a path first");
            return;
        }
        let is_remote = self.target_tab.is_remote;
        if is_remote && !raw_path.starts_with('/') {
            self.error("Remote paths must be absolute (e.g. /etc/nginx/nginx.conf)");
            return;
        }
        let format = match self.check_tab.format_choice.resolve(&raw_path) {
            Some(f) => f,
            None => {
                self.error("Can't tell the format from this filename — set Format to something other than Auto");
                return;
            }
        };
        let read_path = if is_remote { raw_path.clone() } else { crate::config::expand_path(&raw_path) };
        let target = self.target_tab.as_target();
        self.check_tab.checking = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = (|| -> Result<String, String> {
                let session = ExecSession::open(&target)?;
                engine::read_file(&session, &read_path)
            })();
            let _ = tx.send(Msg::CheckedFile { path: read_path, format, result });
        });
    }

    fn trigger_fix(&mut self) {
        let Some(result) = &self.check_tab.result else { return };
        let path = result.path.clone();
        let format = result.format;
        let target = self.target_tab.as_target();
        self.check_tab.fixing = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let outcome = (|| -> Result<FixOutcome, String> {
                let session = ExecSession::open(&target)?;
                let content = engine::read_file(&session, &path)?;
                let Some(fixed) = fixer::try_fix(format, &content) else {
                    return Err("no automatic fix available — the remaining error(s) need manual editing".to_string());
                };
                let backup = engine::write_file_with_backup(&session, &path, &fixed)?;
                let now_valid = format::validate(format, &fixed).is_ok();
                let remaining_error = format::validate(format, &fixed).err();
                Ok(FixOutcome { backup, now_valid, remaining_error })
            })();
            let _ = tx.send(Msg::FixDone { result: outcome });
        });
    }

    // ── Key handling ─────────────────────────────────────────────────
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if let Some((_, _)) = &self.modal {
            match key.code {
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => self.modal = None,
                _ => {}
            }
            return false;
        }

        if self.check_tab.confirm_fix {
            match key.code {
                KeyCode::Enter => {
                    self.check_tab.confirm_fix = false;
                    self.trigger_fix();
                }
                KeyCode::Esc => self.check_tab.confirm_fix = false,
                _ => {}
            }
            return false;
        }

        if self.check_tab.show_results {
            self.handle_results_key(key);
            return false;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('y') {
            let ok = copy_history_to_clipboard(&self.history);
            self.history.push((ok, if ok { "History copied to clipboard".to_string() } else { "Couldn't access the clipboard".to_string() }));
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Up | KeyCode::Down) {
            self.history_scroll = if key.code == KeyCode::Up { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            return false;
        }

        let pickers_open = self.target_tab.host_picker.is_some() || self.target_tab.key_picker.is_some() || self.check_tab.browser.is_some();

        match key.code {
            KeyCode::Esc if !pickers_open => return true,
            KeyCode::F(1) if !pickers_open => {
                self.tab = Tab::Target;
                return false;
            }
            KeyCode::F(2) if !pickers_open => {
                self.tab = Tab::Check;
                return false;
            }
            _ => {}
        }

        match self.tab {
            Tab::Target => self.handle_target_key(key),
            Tab::Check => self.handle_check_key(key),
        }
        false
    }

    fn handle_results_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('y') {
            if let Some(r) = &self.check_tab.result {
                let text = if r.valid { format!("{}: valid {}", r.path, r.format.label()) } else { r.error_text.clone() };
                let ok = Clipboard::new().and_then(|mut c| c.set_text(text)).is_ok();
                self.history.push((ok, if ok { "Result copied to clipboard".to_string() } else { "Couldn't access the clipboard".to_string() }));
            }
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.check_tab.show_results = false;
            }
            KeyCode::Up => self.check_tab.results_scroll = self.check_tab.results_scroll.saturating_add(1),
            KeyCode::Down => self.check_tab.results_scroll = self.check_tab.results_scroll.saturating_sub(1),
            KeyCode::Char('f') | KeyCode::Char('F') => {
                let can_fix = self.check_tab.result.as_ref().is_some_and(|r| !r.valid) && !self.check_tab.fixing;
                if can_fix {
                    self.check_tab.confirm_fix = true;
                }
            }
            _ => {}
        }
    }

    fn handle_target_key(&mut self, key: KeyEvent) {
        if self.target_tab.key_picker.is_some() {
            match key.code {
                KeyCode::Esc => self.target_tab.key_picker = None,
                KeyCode::Up => {
                    if let Some(p) = self.target_tab.key_picker.as_mut() {
                        p.up();
                    }
                }
                KeyCode::Down => {
                    if let Some(p) = self.target_tab.key_picker.as_mut() {
                        p.down();
                    }
                }
                KeyCode::Enter => {
                    let picked = self.target_tab.key_picker.as_mut().and_then(|p| p.activate());
                    if let Some(path) = picked {
                        self.target_tab.ssh_key_path = Input::new(&path.to_string_lossy());
                        self.target_tab.key_picker = None;
                    }
                }
                _ => {}
            }
            return;
        }

        if self.target_tab.host_picker.is_some() {
            match key.code {
                KeyCode::Esc => self.target_tab.host_picker = None,
                KeyCode::Up => {
                    if let Some(p) = self.target_tab.host_picker.as_mut() {
                        p.up();
                    }
                }
                KeyCode::Down => {
                    if let Some(p) = self.target_tab.host_picker.as_mut() {
                        p.down();
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(p) = self.target_tab.host_picker.as_mut() {
                        p.insert(c);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(p) = self.target_tab.host_picker.as_mut() {
                        p.backspace();
                    }
                }
                KeyCode::Enter => {
                    let picked = self.target_tab.host_picker.as_ref().and_then(|p| p.activate());
                    if let Some(server) = picked {
                        self.target_tab.fill_from_host(&server);
                        self.target_tab.host_picker = None;
                    }
                }
                _ => {}
            }
            return;
        }

        if self.target_tab.field == TargetField::Mode {
            match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Enter => {
                    self.target_tab.is_remote = !self.target_tab.is_remote;
                    self.target_tab.connected_label = None;
                    return;
                }
                _ => {}
            }
        }

        let tt = &mut self.target_tab;
        match key.code {
            KeyCode::Tab => tt.next_field(),
            KeyCode::BackTab => tt.prev_field(),
            KeyCode::Up => tt.prev_field(),
            KeyCode::Down => tt.next_field(),
            KeyCode::Enter => match tt.field {
                TargetField::SshHost => tt.host_picker = Some(HostPicker::new()),
                TargetField::SshKeyPath => tt.key_picker = Some(FilePicker::new(tt.ssh_key_path.value())),
                TargetField::BtnConnect => self.trigger_connect(),
                _ => tt.next_field(),
            },
            KeyCode::Char(c) => match tt.field {
                TargetField::SshHost => tt.ssh_host.insert(c),
                TargetField::SshPort => tt.ssh_port.insert(c),
                TargetField::SshUser => tt.ssh_user.insert(c),
                TargetField::SshKeyPath => tt.ssh_key_path.insert(c),
                TargetField::SshPassword => tt.ssh_password.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match tt.field {
                TargetField::SshHost => tt.ssh_host.backspace(),
                TargetField::SshPort => tt.ssh_port.backspace(),
                TargetField::SshUser => tt.ssh_user.backspace(),
                TargetField::SshKeyPath => tt.ssh_key_path.backspace(),
                TargetField::SshPassword => tt.ssh_password.backspace(),
                _ => {}
            },
            KeyCode::Delete => match tt.field {
                TargetField::SshHost => tt.ssh_host.delete(),
                TargetField::SshPort => tt.ssh_port.delete(),
                TargetField::SshUser => tt.ssh_user.delete(),
                TargetField::SshKeyPath => tt.ssh_key_path.delete(),
                TargetField::SshPassword => tt.ssh_password.delete(),
                _ => {}
            },
            KeyCode::Left => match tt.field {
                TargetField::SshHost => tt.ssh_host.left(),
                TargetField::SshPort => tt.ssh_port.left(),
                TargetField::SshUser => tt.ssh_user.left(),
                TargetField::SshKeyPath => tt.ssh_key_path.left(),
                TargetField::SshPassword => tt.ssh_password.left(),
                _ => {}
            },
            KeyCode::Right => match tt.field {
                TargetField::SshHost => tt.ssh_host.right(),
                TargetField::SshPort => tt.ssh_port.right(),
                TargetField::SshUser => tt.ssh_user.right(),
                TargetField::SshKeyPath => tt.ssh_key_path.right(),
                TargetField::SshPassword => tt.ssh_password.right(),
                _ => {}
            },
            KeyCode::Home => match tt.field {
                TargetField::SshHost => tt.ssh_host.home(),
                TargetField::SshPort => tt.ssh_port.home(),
                TargetField::SshUser => tt.ssh_user.home(),
                TargetField::SshKeyPath => tt.ssh_key_path.home(),
                TargetField::SshPassword => tt.ssh_password.home(),
                _ => {}
            },
            KeyCode::End => match tt.field {
                TargetField::SshHost => tt.ssh_host.end_of_line(),
                TargetField::SshPort => tt.ssh_port.end_of_line(),
                TargetField::SshUser => tt.ssh_user.end_of_line(),
                TargetField::SshKeyPath => tt.ssh_key_path.end_of_line(),
                TargetField::SshPassword => tt.ssh_password.end_of_line(),
                _ => {}
            },
            _ => {}
        }
    }

    fn handle_check_key(&mut self, key: KeyEvent) {
        if self.check_tab.browser.is_some() {
            match key.code {
                KeyCode::Esc => self.check_tab.browser = None,
                KeyCode::Up => {
                    if let Some(b) = self.check_tab.browser.as_mut() {
                        b.up();
                    }
                }
                KeyCode::Down => {
                    if let Some(b) = self.check_tab.browser.as_mut() {
                        b.down();
                    }
                }
                KeyCode::Enter => {
                    let action = self.check_tab.browser.as_ref().map(Browser::on_enter);
                    match action {
                        Some(BrowserAction::Navigate(dir)) => {
                            if let Some(b) = self.check_tab.browser.as_mut() {
                                b.cwd = dir.clone();
                                b.loading = true;
                                b.entries.clear();
                            }
                            self.list_dir(dir);
                        }
                        Some(BrowserAction::Picked(path)) => {
                            self.check_tab.path = Input::new(&path);
                            self.check_tab.browser = None;
                            self.trigger_check();
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            return;
        }

        if self.check_tab.field == CheckField::Format {
            match key.code {
                KeyCode::Left => {
                    self.check_tab.format_choice = self.check_tab.format_choice.cycle(-1);
                    return;
                }
                KeyCode::Right | KeyCode::Enter => {
                    self.check_tab.format_choice = self.check_tab.format_choice.cycle(1);
                    return;
                }
                _ => {}
            }
        }

        let ct = &mut self.check_tab;
        match key.code {
            KeyCode::Tab => ct.next_field(),
            KeyCode::BackTab => ct.prev_field(),
            KeyCode::Up => ct.prev_field(),
            KeyCode::Down => ct.next_field(),
            KeyCode::Enter => match ct.field {
                // Enter on Path opens the browser — same as pressing
                // Browse… — rather than just moving focus, since the
                // common case is "I don't have a path typed/pasted yet, I
                // want to pick one". Typing or pasting a path directly
                // still works from this field either way (Ctrl+V / a
                // bracketed paste inserts text, doesn't submit).
                CheckField::Path | CheckField::BtnBrowse => self.start_browse(),
                CheckField::BtnCheck => self.trigger_check(),
                _ => ct.next_field(),
            },
            KeyCode::Char(c) if ct.field == CheckField::Path => ct.path.insert(c),
            KeyCode::Backspace if ct.field == CheckField::Path => ct.path.backspace(),
            KeyCode::Delete if ct.field == CheckField::Path => ct.path.delete(),
            KeyCode::Left if ct.field == CheckField::Path => ct.path.left(),
            KeyCode::Right if ct.field == CheckField::Path => ct.path.right(),
            KeyCode::Home if ct.field == CheckField::Path => ct.path.home(),
            KeyCode::End if ct.field == CheckField::Path => ct.path.end_of_line(),
            _ => {}
        }
    }

    // ── Mouse handling ───────────────────────────────────────────────
    pub fn handle_mouse(&mut self, me: MouseEvent, area: Rect) {
        if self.modal.is_some() {
            if mouse::left_click(&me).is_some() {
                self.modal = None;
            }
            return;
        }
        // The auto-fix confirmation writes to a file — keyboard-only on
        // purpose, same reasoning as Kernel Tuner's risky-change confirm:
        // a stray click can never trigger it.
        if self.check_tab.confirm_fix {
            return;
        }
        if self.check_tab.show_results {
            self.handle_results_mouse(me, area);
            return;
        }

        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(area);

        if let Some((x, y)) = mouse::left_click(&me) {
            if let Some(i) = mouse::label_row_hit(x, y, chunks[0], &["F1 Target", "F2 Check"]) {
                self.tab = if i == 0 { Tab::Target } else { Tab::Check };
                return;
            }
        }

        match self.tab {
            Tab::Target => self.handle_target_mouse(me, chunks[1]),
            Tab::Check => self.handle_check_mouse(me, chunks[1]),
        }
    }

    fn handle_results_mouse(&mut self, me: MouseEvent, _area: Rect) {
        if let Some(delta) = mouse::scroll_delta(&me) {
            if delta < 0 {
                self.check_tab.results_scroll = self.check_tab.results_scroll.saturating_add(1);
            } else {
                self.check_tab.results_scroll = self.check_tab.results_scroll.saturating_sub(1);
            }
        }
    }

    fn handle_target_mouse(&mut self, me: MouseEvent, area: Rect) {
        if self.target_tab.host_picker.is_some() {
            if let Some((x, y)) = mouse::left_click(&me) {
                if let Some(idx) = self.target_tab.host_picker.as_ref().and_then(|p| p.row_at(area, x, y)) {
                    self.target_tab.host_picker.as_mut().unwrap().selected = idx;
                    if let Some(server) = self.target_tab.host_picker.as_ref().unwrap().activate() {
                        self.target_tab.fill_from_host(&server);
                        self.target_tab.host_picker = None;
                    }
                }
                return;
            }
            if let Some(delta) = mouse::scroll_delta(&me) {
                let p = self.target_tab.host_picker.as_mut().unwrap();
                if delta < 0 {
                    p.up();
                } else {
                    p.down();
                }
            }
            return;
        }
        if self.target_tab.key_picker.is_some() {
            if let Some((x, y)) = mouse::left_click(&me) {
                if let Some(idx) = self.target_tab.key_picker.as_ref().and_then(|p| p.row_at(area, x, y)) {
                    self.target_tab.key_picker.as_mut().unwrap().selected = idx;
                    if let Some(path) = self.target_tab.key_picker.as_mut().unwrap().activate() {
                        self.target_tab.ssh_key_path = Input::new(&path.to_string_lossy());
                        self.target_tab.key_picker = None;
                    }
                }
            } else if let Some(delta) = mouse::scroll_delta(&me) {
                let p = self.target_tab.key_picker.as_mut().unwrap();
                if delta < 0 {
                    p.up();
                } else {
                    p.down();
                }
            }
            return;
        }

        let is_remote = self.target_tab.is_remote;
        let form_height = if is_remote { 20 } else { 10 };
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(form_height), Constraint::Min(3), Constraint::Length(8)]).split(area);

        if let Some(delta) = mouse::scroll_delta(&me) {
            if mouse::in_rect(chunks[2], me.column, me.row) {
                self.history_scroll = if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };

        let form_inner = mouse::block_inner(chunks[0]);
        let rows = target_form_rows(form_inner, is_remote);

        if let Some(i) = mouse::label_row_hit(x, y, rows[0], &["Local", "Remote"]) {
            self.target_tab.field = TargetField::Mode;
            self.target_tab.is_remote = i == 1;
            self.target_tab.connected_label = None;
            return;
        }

        let connect_row = if is_remote { rows[12] } else { rows[2] };
        if mouse::button_row_hit(x, y, connect_row, &["Connect"]).is_some() {
            self.trigger_connect();
            return;
        }

        if is_remote {
            let field_rows: &[(usize, TargetField)] = &[
                (2, TargetField::SshHost),
                (4, TargetField::SshPort),
                (6, TargetField::SshUser),
                (8, TargetField::SshKeyPath),
                (10, TargetField::SshPassword),
            ];
            for (i, field) in field_rows {
                if mouse::in_rect(rows[*i], x, y) {
                    match field {
                        TargetField::SshHost => self.target_tab.host_picker = Some(HostPicker::new()),
                        TargetField::SshKeyPath => self.target_tab.key_picker = Some(FilePicker::new(self.target_tab.ssh_key_path.value())),
                        _ => self.target_tab.field = *field,
                    }
                    return;
                }
            }
        }
    }

    fn handle_check_mouse(&mut self, me: MouseEvent, area: Rect) {
        if self.check_tab.browser.is_some() {
            if let Some((x, y)) = mouse::left_click(&me) {
                let idx = self.check_tab.browser.as_ref().and_then(|b| b.row_at(area, x, y));
                if let Some(idx) = idx {
                    if let Some(b) = self.check_tab.browser.as_mut() {
                        b.selected = idx;
                    }
                    let action = self.check_tab.browser.as_ref().map(Browser::on_enter);
                    match action {
                        Some(BrowserAction::Navigate(dir)) => {
                            if let Some(b) = self.check_tab.browser.as_mut() {
                                b.cwd = dir.clone();
                                b.loading = true;
                                b.entries.clear();
                            }
                            self.list_dir(dir);
                        }
                        Some(BrowserAction::Picked(path)) => {
                            self.check_tab.path = Input::new(&path);
                            self.check_tab.browser = None;
                            self.trigger_check();
                        }
                        _ => {}
                    }
                }
            } else if let Some(delta) = mouse::scroll_delta(&me) {
                let b = self.check_tab.browser.as_mut().unwrap();
                if delta < 0 {
                    b.up();
                } else {
                    b.down();
                }
            }
            return;
        }

        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(10), Constraint::Min(3), Constraint::Length(8)]).split(area);

        if let Some(delta) = mouse::scroll_delta(&me) {
            if mouse::in_rect(chunks[2], me.column, me.row) {
                self.history_scroll = if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };

        let form_inner = mouse::block_inner(chunks[0]);
        let rows = check_form_rows(form_inner);

        if mouse::in_rect(rows[0], x, y) {
            self.check_tab.field = CheckField::Path;
            return;
        }
        if mouse::in_rect(rows[2], x, y) {
            self.check_tab.field = CheckField::Format;
            return;
        }
        if let Some(i) = mouse::button_row_hit(x, y, rows[4], &["Browse…", "Check Syntax"]) {
            if i == 0 {
                self.start_browse();
            } else {
                self.trigger_check();
            }
        }
    }

    // ── Drawing ──────────────────────────────────────────────────────
    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(area);

        let tab_bar = Line::from(vec![
            tab_span("F1 Target", self.tab == Tab::Target),
            Span::styled("  ", Style::default().bg(bg())),
            tab_span("F2 Check", self.tab == Tab::Check),
            Span::styled("  ", Style::default().bg(bg())),
            Span::styled("Esc back  Ctrl+C quit", Style::default().fg(fg2()).bg(bg())),
        ]);
        f.render_widget(Paragraph::new(tab_bar), chunks[0]);

        match self.tab {
            Tab::Target => self.draw_target(f, chunks[1]),
            Tab::Check => self.draw_check(f, chunks[1]),
        }

        if self.check_tab.show_results {
            if let Some(result) = &self.check_tab.result {
                draw_results(f, result, self.check_tab.results_scroll, self.check_tab.fixing, area);
            }
        }
        if self.check_tab.confirm_fix {
            if let Some(result) = &self.check_tab.result {
                draw_confirm_fix(f, &result.path, area);
            }
        }
        if let Some((title, msg)) = &self.modal {
            draw_modal(f, title, msg, area);
        }
    }

    fn draw_target(&self, f: &mut Frame, area: Rect) {
        let is_remote = self.target_tab.is_remote;
        let form_height = if is_remote { 20 } else { 10 };
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(form_height), Constraint::Min(3), Constraint::Length(8)]).split(area);

        let form_block = theme_block(" Target ");
        let form_inner = form_block.inner(chunks[0]);
        f.render_widget(form_block, chunks[0]);
        let rows = target_form_rows(form_inner, is_remote);
        let tt = &self.target_tab;

        f.render_widget(
            Line::from(vec![Span::styled("Mode: ", lbl()), tab_span("Local", !is_remote), Span::raw("  "), tab_span("Remote", is_remote)]),
            rows[0],
        );

        if is_remote {
            let w = |r: Rect| (r.width as usize).saturating_sub(12).max(6);
            f.render_widget(
                Line::from(vec![Span::styled("SSH Host   ", lbl()), input_span(&tt.ssh_host, tt.field == TargetField::SshHost, false, w(rows[2]))]),
                rows[2],
            );
            f.render_widget(
                Line::from(vec![Span::styled("SSH Port   ", lbl()), input_span(&tt.ssh_port, tt.field == TargetField::SshPort, false, w(rows[4]))]),
                rows[4],
            );
            f.render_widget(
                Line::from(vec![Span::styled("SSH User   ", lbl()), input_span(&tt.ssh_user, tt.field == TargetField::SshUser, false, w(rows[6]))]),
                rows[6],
            );
            f.render_widget(
                Line::from(vec![Span::styled("Key Path   ", lbl()), input_span(&tt.ssh_key_path, tt.field == TargetField::SshKeyPath, false, w(rows[8]))]),
                rows[8],
            );
            f.render_widget(
                Line::from(vec![Span::styled("Password   ", lbl()), input_span(&tt.ssh_password, tt.field == TargetField::SshPassword, true, w(rows[10]))]),
                rows[10],
            );
        }

        let btn_row = if is_remote { rows[12] } else { rows[2] };
        f.render_widget(Line::from(vec![btn_span("Connect", tt.field == TargetField::BtnConnect)]), btn_row);

        let hint_row = if is_remote { rows[14] } else { rows[4] };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("Tab/Shift+Tab move  Left/Right toggle Mode  Enter activate  Esc back", lbl()))),
            hint_row,
        );

        if let Some(picker) = &tt.host_picker {
            host_picker::draw(f, picker, chunks[1]);
        } else if let Some(picker) = &tt.key_picker {
            file_picker::draw(f, picker, chunks[1]);
        } else {
            let info_block = theme_block(" Target Info ");
            let info_inner = info_block.inner(chunks[1]);
            f.render_widget(info_block, chunks[1]);
            let line = if tt.connecting {
                Line::from(Span::styled("Connecting…", Style::default().fg(yellow())))
            } else if let Some(label) = &tt.connected_label {
                Line::from(vec![Span::styled("Connected to: ", lbl()), Span::styled(label.clone(), Style::default().fg(green()))])
            } else {
                Line::from(Span::styled(
                    "Not connected yet. Choose Local or fill in a remote host above, then press Connect — or just go to the Check tab, a connection is opened automatically when you browse or check a file.",
                    lbl(),
                ))
            };
            f.render_widget(Paragraph::new(line).wrap(Wrap { trim: false }), info_inner);
        }

        draw_history(f, &self.history, chunks[2], self.history_scroll);
    }

    fn draw_check(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(10), Constraint::Min(3), Constraint::Length(8)]).split(area);

        let form_block = theme_block(" Check ");
        let form_inner = form_block.inner(chunks[0]);
        f.render_widget(form_block, chunks[0]);
        let rows = check_form_rows(form_inner);
        let ct = &self.check_tab;

        let w = (rows[0].width as usize).saturating_sub(12).max(6);
        f.render_widget(
            Line::from(vec![Span::styled("Path       ", lbl()), input_span(&ct.path, ct.field == CheckField::Path, false, w)]),
            rows[0],
        );
        f.render_widget(
            Line::from(vec![
                Span::styled("Format     ", lbl()),
                Span::styled(
                    format!("< {} >", ct.format_choice.label()),
                    if ct.field == CheckField::Format { focused() } else { normal() },
                ),
                Span::styled("   (Auto detects from the extension — Left/Right to force one)", lbl()),
            ]),
            rows[2],
        );
        f.render_widget(
            Line::from(vec![
                btn_span("Browse…", ct.field == CheckField::BtnBrowse),
                Span::raw("  "),
                btn_span(if ct.checking { "Checking…" } else { "Check Syntax" }, ct.field == CheckField::BtnCheck),
            ]),
            rows[4],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Tab/Shift+Tab move  Enter activate  Ctrl+Y copy History  Ctrl+\u{2191}/\u{2193} scroll History  Esc back",
                lbl(),
            ))),
            rows[6],
        );

        if let Some(browser) = &ct.browser {
            draw_browser(f, browser, chunks[1]);
        } else {
            let info_block = theme_block(" Last Result ");
            let info_inner = info_block.inner(chunks[1]);
            f.render_widget(info_block, chunks[1]);
            let line = match &ct.result {
                Some(r) if r.valid => Line::from(vec![
                    Span::styled("\u{2713} ", Style::default().fg(green())),
                    Span::raw(r.path.clone()),
                    Span::styled(format!(" — valid {}", r.format.label()), lbl()),
                ]),
                Some(r) => Line::from(vec![
                    Span::styled("\u{2717} ", Style::default().fg(red())),
                    Span::raw(r.path.clone()),
                    Span::styled(format!(" — invalid {} (Esc back then re-check to reopen the details)", r.format.label()), lbl()),
                ]),
                None => Line::from(Span::styled(
                    "Type a path (or Browse…) and press Check Syntax. Supported: .json .toml .yaml/.yml .xml.",
                    lbl(),
                )),
            };
            f.render_widget(Paragraph::new(line).wrap(Wrap { trim: false }), info_inner);
        }

        draw_history(f, &self.history, chunks[2], self.history_scroll);
    }
}

fn draw_results(f: &mut Frame, result: &CheckResult, scroll: u16, fixing: bool, area: Rect) {
    let width = area.width.saturating_sub(6).min(120);
    let height = area.height.saturating_sub(4).min(44);
    let modal_area = centered_rect(width, height, area);

    f.render_widget(Clear, modal_area);

    let (status_color, status_word) = if result.valid { (green(), "VALID") } else { (red(), "INVALID") };
    let title = format!(" {} — {} — {} ", result.format.label(), status_word, result.path);
    let block = Block::default()
        .title(Span::styled(title, Style::default().fg(status_color).add_modifier(Modifier::BOLD)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(status_color))
        .style(Style::default().bg(bg2()));
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    let rows = Layout::default().direction(Direction::Vertical).margin(1).constraints([Constraint::Min(3), Constraint::Length(1)]).split(inner);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(msg) = &result.fix_message {
        lines.push(Line::from(Span::styled(msg.as_str(), Style::default().fg(if result.valid { green() } else { yellow() }))));
        if let Some(backup) = &result.fix_backup {
            lines.push(Line::from(Span::styled(format!("Original backed up to {backup}"), lbl())));
        }
        lines.push(Line::from(""));
    }
    if result.valid {
        lines.push(Line::from(Span::styled("No syntax errors found.", Style::default().fg(green()))));
    } else {
        for l in result.error_text.lines() {
            lines.push(Line::from(Span::raw(l.to_string())));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "(the parser stops at the first error — fixing it may reveal more on the next check)",
            lbl(),
        )));
    }

    let total = lines.len() as u16;
    let visible = rows[0].height;
    let max_offset = total.saturating_sub(visible);
    let offset = scroll.min(max_offset);
    let body_scroll = max_offset - offset;
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((body_scroll, 0)), rows[0]);

    let hint = if fixing {
        "Applying fix…".to_string()
    } else if result.valid {
        "\u{2191}\u{2193} scroll  Ctrl+Y copy  Esc close".to_string()
    } else {
        "\u{2191}\u{2193} scroll  Ctrl+Y copy  F try auto-fix  Esc close".to_string()
    };
    f.render_widget(Paragraph::new(Line::from(Span::styled(hint, lbl()))), rows[1]);
}

/// How many visual rows `text` (possibly containing its own `\n`s) takes up
/// once greedily word-wrapped at `width` columns — same algorithm as
/// `home::wrap_text`, kept local here since it's only needed for sizing
/// this one modal. An empty raw line (from `\n\n`) still counts as one row,
/// matching how `Paragraph`'s own `Wrap` renders a blank line.
fn wrapped_line_count(text: &str, width: usize) -> usize {
    let width = width.max(1);
    let mut total = 0usize;
    for raw_line in text.split('\n') {
        if raw_line.trim().is_empty() {
            total += 1;
            continue;
        }
        let mut cur_len = 0usize;
        let mut rows = 1usize;
        for word in raw_line.split_whitespace() {
            let word_len = word.chars().count();
            if cur_len == 0 {
                cur_len = word_len;
            } else if cur_len + 1 + word_len <= width {
                cur_len += 1 + word_len;
            } else {
                rows += 1;
                cur_len = word_len;
            }
        }
        total += rows;
    }
    total.max(1)
}

fn draw_confirm_fix(f: &mut Frame, path: &str, area: Rect) {
    let msg = format!(
        "Try to automatically fix common syntax mistakes in:\n  {path}\n\n\
         This is a best-effort mechanical fix — stray trailing commas and comments \
         in JSON, smart quotes, tab indentation in YAML, unescaped '&' in XML. It \
         will NOT reorder or invent data, and it may not fix everything.\n\n\
         The current file is backed up first (same path, .atk-bak-<timestamp> \
         suffix), then the fixed version is written and re-checked so you can see \
         what — if anything — still needs fixing by hand.\n\n\
         Enter to continue, Esc to cancel. (Keyboard only — a stray click can't \
         confirm this.)"
    );
    let width = 72u16.min(area.width.saturating_sub(4));
    // `msg.lines().count()` only counts the literal `\n`s in the source —
    // it has no idea the Paragraph below will word-wrap the long
    // paragraphs (and the path, which can be arbitrarily long) across
    // several more rows than that. Sizing the box off the raw count made
    // it too short, silently clipping the bottom of the message — in
    // practice, the "Enter to continue, Esc to cancel" line, which made it
    // look like the modal had no way to confirm or cancel at all.
    let text_width = width.saturating_sub(2) as usize; // inside the block's left/right border
    // `+ 4` rather than the `+ 2` that would exactly match "content rows +
    // top/bottom border": `wrapped_line_count`'s greedy wrap is a
    // hand-rolled approximation of ratatui's own, and in practice undercounts
    // by a row or so — cheaper to over-allocate a little blank space at the
    // bottom than to clip the "Enter to continue, Esc to cancel" line again.
    // `.max(18)` is a floor for the common case (a normal-length path) so
    // the box doesn't look cramped even when the estimate comes in low.
    let height = (wrapped_line_count(&msg, text_width) as u16 + 4).max(18).min(area.height.saturating_sub(2)).max(10);
    let modal_area = centered_rect(width, height, area);
    f.render_widget(Clear, modal_area);
    f.render_widget(
        Paragraph::new(msg)
            .block(
                Block::default()
                    .title(Span::styled(" Confirm Auto-Fix ", Style::default().fg(yellow())))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(yellow())),
            )
            .style(Style::default().fg(fg()).bg(bg2()))
            .wrap(Wrap { trim: true }),
        modal_area,
    );
}
