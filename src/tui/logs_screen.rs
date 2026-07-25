//! Logs & Journals reader — SSH into a server with a saved key/password
//! profile and read its systemd journal or a plain file under `/var/log`
//! (or anywhere else), with severity filtering and text search, for
//! troubleshooting without needing a separate terminal + `ssh` session.

use std::{sync::mpsc, thread, time::Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};

use crate::logs_mgr::{
    client::{self, Priority, RemoteEntry, Source},
    config::{self, Connection, ConnectionInput, ConnectionWithSecrets},
};
use crate::ssh_exec::one_line;

use super::file_picker::FilePicker;
use super::host_picker::HostPicker;
use super::mouse;
use super::widgets::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Connections,
    Viewer,
}

// ── Connections tab ──────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnField {
    Table,
    Label,
    Host,
    SshPort,
    SshUser,
    SshKeyPath,
    SshPassword,
    BtnSave,
    BtnNew,
    BtnDelete,
    BtnTest,
}

const CONN_FIELDS: &[ConnField] = &[
    ConnField::Table,
    ConnField::Label,
    ConnField::Host,
    ConnField::SshPort,
    ConnField::SshUser,
    ConnField::SshKeyPath,
    ConnField::SshPassword,
    ConnField::BtnSave,
    ConnField::BtnNew,
    ConnField::BtnDelete,
    ConnField::BtnTest,
];

struct ConnectionsTab {
    selected: Option<usize>,
    table_idx: usize,
    label: Input,
    host: Input,
    ssh_port: Input,
    ssh_user: Input,
    ssh_key_path: Input,
    ssh_password: Input,
    field: ConnField,
    key_picker: Option<FilePicker>,
    host_picker: Option<HostPicker>,
}

impl ConnectionsTab {
    fn new() -> Self {
        Self {
            selected: None,
            table_idx: 0,
            label: Input::default(),
            host: Input::default(),
            ssh_port: Input::new("22"),
            ssh_user: Input::default(),
            ssh_key_path: Input::default(),
            ssh_password: Input::default(),
            field: ConnField::Table,
            key_picker: None,
            host_picker: None,
        }
    }

    fn next_field(&mut self) {
        let idx = CONN_FIELDS.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = CONN_FIELDS[(idx + 1) % CONN_FIELDS.len()];
    }
    fn prev_field(&mut self) {
        let idx = CONN_FIELDS.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = CONN_FIELDS[(idx + CONN_FIELDS.len() - 1) % CONN_FIELDS.len()];
    }

    fn clear_form(&mut self) {
        self.selected = None;
        self.label = Input::default();
        self.host = Input::default();
        self.ssh_port = Input::new("22");
        self.ssh_user = Input::default();
        self.ssh_key_path = Input::default();
        self.ssh_password = Input::default();
        self.key_picker = None;
        self.host_picker = None;
    }

    fn load_from(&mut self, idx: usize, c: &Connection) {
        self.selected = Some(idx);
        self.label = Input::new(&c.label);
        self.host = Input::new(&c.host);
        self.ssh_port = Input::new(&c.ssh_port);
        self.ssh_user = Input::new(&c.ssh_user);
        self.ssh_key_path = Input::new(&c.ssh_key_path);
        self.ssh_password = Input::default();
        self.key_picker = None;
        self.host_picker = None;
    }

    /// Fills the SSH side of the form from a host already known to the
    /// SSH Server Manager, so it doesn't need typing twice. `Label`
    /// defaults to the host's alias if it's still empty.
    fn fill_from_host(&mut self, server: &crate::easyssh_mgr::config::Server) {
        if self.label.value().trim().is_empty() {
            self.label = Input::new(&server.alias);
        }
        self.host = Input::new(server.effective_host());
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

    fn as_input(&self) -> ConnectionInput {
        ConnectionInput {
            label: self.label.value().trim().to_string(),
            host: self.host.value().trim().to_string(),
            ssh_port: self.ssh_port.value().trim().to_string(),
            ssh_user: self.ssh_user.value().trim().to_string(),
            ssh_key_path: self.ssh_key_path.value().trim().to_string(),
            ssh_password: self.ssh_password.value().to_string(),
        }
    }
}

// ── Viewer tab ───────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewerField {
    Connection,
    Source,
    UnitOrPath,
    Since,
    Priority,
    Lines,
    Search,
    BtnBrowse,
    BtnFetch,
    AutoRefresh,
    LogPane,
}

fn viewer_active_fields(source: Source) -> Vec<ViewerField> {
    let mut v = vec![ViewerField::Connection, ViewerField::Source, ViewerField::UnitOrPath];
    if source == Source::Journal {
        v.push(ViewerField::Since);
    }
    v.push(ViewerField::Priority);
    v.push(ViewerField::Lines);
    v.push(ViewerField::Search);
    if source == Source::File {
        v.push(ViewerField::BtnBrowse);
    }
    v.push(ViewerField::BtnFetch);
    v.push(ViewerField::AutoRefresh);
    v.push(ViewerField::LogPane);
    v
}

/// The Filters form's row constraints and overall height — shared between
/// `draw_viewer` (to render) and `handle_viewer_mouse` (to hit-test
/// clicks), so the `Source`-dependent row count can't drift between the
/// two the way independently-hand-copied `Layout::split` calls could. Row
/// indices for each field are fixed given `source` — see the comments in
/// `handle_viewer_mouse`.
fn viewer_form_constraints(source: Source) -> (Vec<Constraint>, u16) {
    let mut constraints = vec![
        Constraint::Length(1), // 0 Connection
        Constraint::Length(1), // 1 spacer
        Constraint::Length(1), // 2 Source
        Constraint::Length(1), // 3 spacer
        Constraint::Length(1), // 4 Unit/Path
        Constraint::Length(1), // 5 spacer
    ];
    if source == Source::Journal {
        constraints.extend([Constraint::Length(1), Constraint::Length(1)]); // Since + spacer
    }
    constraints.extend([
        Constraint::Length(1), // Priority
        Constraint::Length(1), // spacer
        Constraint::Length(1), // Lines
        Constraint::Length(1), // spacer
        Constraint::Length(1), // Search
        Constraint::Length(1), // spacer
        Constraint::Length(1), // Auto-refresh
        Constraint::Length(1), // spacer
        Constraint::Length(1), // buttons
        Constraint::Length(1), // spacer
        Constraint::Length(1), // nav hint
    ]);
    let content_rows = constraints.len() as u16;
    constraints.push(Constraint::Min(0));
    let form_height = content_rows + 4;
    (constraints, form_height)
}

const AUTO_REFRESH_SECS: u64 = 5;

struct RemoteBrowser {
    path: String,
    entries: Vec<RemoteEntry>,
    selected: usize,
    loading: bool,
    error: Option<String>,
}

struct ViewerTab {
    connection_input: Input,
    connection_dropdown_open: bool,
    connection_idx: usize,
    source: Source,
    unit: Input,
    path: Input,
    since: Input,
    priority_idx: usize,
    lines: Input,
    search: Input,
    auto_refresh: bool,
    field: ViewerField,
    log_lines: Vec<String>,
    scroll: u16,
    fetching: bool,
    last_fetch: Option<Instant>,
    browser: Option<RemoteBrowser>,
}

impl ViewerTab {
    fn new() -> Self {
        Self {
            connection_input: Input::default(),
            connection_dropdown_open: false,
            connection_idx: 0,
            source: Source::Journal,
            unit: Input::default(),
            path: Input::new("/var/log/syslog"),
            since: Input::default(),
            priority_idx: 0,
            lines: Input::new("200"),
            search: Input::default(),
            auto_refresh: false,
            field: ViewerField::Connection,
            log_lines: Vec::new(),
            scroll: 0,
            fetching: false,
            last_fetch: None,
            browser: None,
        }
    }

    fn next_field(&mut self) {
        let fields = viewer_active_fields(self.source);
        let idx = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = fields[(idx + 1) % fields.len()];
    }
    fn prev_field(&mut self) {
        let fields = viewer_active_fields(self.source);
        let idx = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = fields[(idx + fields.len() - 1) % fields.len()];
    }

    fn priority(&self) -> Priority {
        client::PRIORITIES[self.priority_idx]
    }
}

enum Msg {
    Log(bool, String),
    FetchResult(Result<Vec<String>, String>),
    BrowseResult(String, Result<Vec<RemoteEntry>, String>),
}

pub struct LogsScreen {
    tab: Tab,
    connections_tab: ConnectionsTab,
    viewer_tab: ViewerTab,
    modal: Option<(String, String)>,
    cfg: config::Config,
    history: Vec<(bool, String)>,
    /// Lines scrolled up from the newest entry in the History panel — see `widgets::draw_history`.
    history_scroll: u16,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
}

impl LogsScreen {
    pub fn new() -> Self {
        let cfg = config::load().unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        Self {
            tab: Tab::Connections,
            connections_tab: ConnectionsTab::new(),
            viewer_tab: ViewerTab::new(),
            modal: None,
            cfg,
            history: Vec::new(),
            history_scroll: 0,
            tx,
            rx,
        }
    }

    pub fn tick(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Log(ok, line) => self.history.push((ok, line)),
                Msg::FetchResult(result) => {
                    self.viewer_tab.fetching = false;
                    self.viewer_tab.last_fetch = Some(Instant::now());
                    match result {
                        Ok(lines) => {
                            self.history.push((true, format!("Fetched {} line(s)", lines.len())));
                            self.viewer_tab.log_lines = lines;
                            self.viewer_tab.scroll = 0;
                        }
                        Err(e) => self.history.push((false, one_line(&e))),
                    }
                }
                Msg::BrowseResult(path, result) => {
                    if let Some(b) = &mut self.viewer_tab.browser {
                        b.loading = false;
                        match result {
                            Ok(entries) => {
                                b.path = path;
                                b.entries = entries;
                                b.selected = 0;
                                b.error = None;
                            }
                            Err(e) => b.error = Some(one_line(&e)),
                        }
                    }
                }
            }
        }

        if self.viewer_tab.auto_refresh && !self.viewer_tab.fetching {
            let due = self.viewer_tab.last_fetch.map(|t| t.elapsed().as_secs() >= AUTO_REFRESH_SECS).unwrap_or(true);
            if due && !self.viewer_tab.connection_input.value().trim().is_empty() {
                self.trigger_fetch();
            }
        }
    }

    fn connection_by_label(&self, label: &str) -> Option<ConnectionWithSecrets> {
        if let Some(c) = self.cfg.connections.iter().find(|c| c.label == label) {
            return self.cfg.with_secrets(&c.id);
        }
        // Falls back to a host known to the SSH Server Manager
        // (~/.ssh/config) that was never separately saved as a Logs
        // profile — any host you can already `ssh` into should be usable
        // here without a "create a profile first" step. No password:
        // easyssh only ever has key-based auth on file.
        let server = crate::easyssh_mgr::config::list_servers("").ok()?.into_iter().find(|s| s.alias == label)?;
        let host = server.effective_host().to_string();
        Some(ConnectionWithSecrets {
            id: String::new(),
            label: server.alias,
            host,
            ssh_port: if server.port == 0 { "22".to_string() } else { server.port.to_string() },
            ssh_user: server.user,
            ssh_key_path: server.identity_files.into_iter().next().unwrap_or_default(),
            ssh_password: String::new(),
        })
    }

    /// SSH Server Manager hosts not already covered by a saved Logs
    /// profile (matched by host, so re-saving one with a password doesn't
    /// leave a redundant duplicate row) — shown in the Connections table
    /// and offered in the Viewer's Connection field so a host "just
    /// works" the moment it's in `~/.ssh/config`, no manual Add/Save step
    /// required first.
    fn known_ssh_extras(&self) -> Vec<crate::easyssh_mgr::config::Server> {
        let servers = crate::easyssh_mgr::config::list_servers("").unwrap_or_default();
        servers.into_iter().filter(|s| !self.cfg.connections.iter().any(|c| c.host == s.effective_host())).collect()
    }

    /// Surfaces an error both as a modal (seen right away) and as a
    /// permanent History entry (still there once the modal is dismissed).
    fn error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.history.push((false, msg.clone()));
        self.modal = Some(("Error".into(), msg));
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.modal.is_some() {
            match key.code {
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => self.modal = None,
                _ => {}
            }
            return false;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('y') {
            let ok = copy_history_to_clipboard(&self.history);
            self.history.push((ok, if ok { "History copied to clipboard".to_string() } else { "Couldn't access the clipboard".to_string() }));
            return false;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Up | KeyCode::Down) {
            self.history_scroll =
                if key.code == KeyCode::Up { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            return false;
        }

        if self.viewer_tab.browser.is_some() {
            self.handle_browser_key(key);
            return false;
        }

        match key.code {
            KeyCode::Esc
                if !self.viewer_tab.connection_dropdown_open
                    && self.connections_tab.key_picker.is_none()
                    && self.connections_tab.host_picker.is_none() =>
            {
                return true
            }
            KeyCode::F(1) => {
                self.tab = Tab::Connections;
                return false;
            }
            KeyCode::F(2) => {
                self.tab = Tab::Viewer;
                return false;
            }
            _ => {}
        }

        match self.tab {
            Tab::Connections => self.handle_connections_key(key),
            Tab::Viewer => self.handle_viewer_key(key),
        }
        false
    }

    pub fn handle_mouse(&mut self, me: MouseEvent, area: Rect) {
        if self.modal.is_some() {
            // Otherwise one failed action silently eats every click for
            // the rest of the session, indistinguishable from "the mouse
            // stopped working" — a click dismisses it, same as Enter/Esc.
            if mouse::left_click(&me).is_some() {
                self.modal = None;
            }
            return;
        }
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(area);

        if let Some((x, y)) = mouse::left_click(&me) {
            if let Some(i) = mouse::label_row_hit(x, y, chunks[0], &["F1 Connections", "F2 Viewer"]) {
                self.tab = if i == 0 { Tab::Connections } else { Tab::Viewer };
                return;
            }
        }

        match self.tab {
            Tab::Connections => self.handle_connections_mouse(me, chunks[1]),
            Tab::Viewer => self.handle_viewer_mouse(me, chunks[1]),
        }
    }

    fn handle_connections_mouse(&mut self, me: MouseEvent, area: Rect) {
        let ct = &self.connections_tab;
        if let Some(picker) = &ct.host_picker {
            if let Some((x, y)) = mouse::left_click(&me) {
                if let Some(idx) = picker.row_at(area, x, y) {
                    self.connections_tab.host_picker.as_mut().unwrap().selected = idx;
                    if let Some(server) = self.connections_tab.host_picker.as_ref().unwrap().activate() {
                        self.connections_tab.fill_from_host(&server);
                        self.connections_tab.host_picker = None;
                    }
                }
                return;
            }
            if let Some(delta) = mouse::scroll_delta(&me) {
                let p = self.connections_tab.host_picker.as_mut().unwrap();
                if delta < 0 {
                    p.up();
                } else {
                    p.down();
                }
            }
            return;
        }
        if ct.key_picker.is_some() {
            if let Some((x, y)) = mouse::left_click(&me) {
                if let Some(idx) = self.connections_tab.key_picker.as_ref().and_then(|p| p.row_at(area, x, y)) {
                    self.connections_tab.key_picker.as_mut().unwrap().selected = idx;
                    if let Some(path) = self.connections_tab.key_picker.as_mut().unwrap().activate() {
                        self.connections_tab.ssh_key_path = Input::new(&path.to_string_lossy());
                        self.connections_tab.key_picker = None;
                    }
                }
            } else if let Some(delta) = mouse::scroll_delta(&me) {
                let p = self.connections_tab.key_picker.as_mut().unwrap();
                if delta < 0 {
                    p.up();
                } else {
                    p.down();
                }
            }
            return;
        }

        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(0), Constraint::Length(22), Constraint::Length(6)]).split(area);

        let saved_len = self.cfg.connections.len();
        let extras = self.known_ssh_extras();
        let total = saved_len + extras.len();

        if let Some(delta) = mouse::scroll_delta(&me) {
            if total > 0 && mouse::in_rect(chunks[0], me.column, me.row) {
                if delta < 0 && self.connections_tab.table_idx > 0 {
                    self.connections_tab.table_idx -= 1;
                } else if delta > 0 && self.connections_tab.table_idx + 1 < total {
                    self.connections_tab.table_idx += 1;
                }
            } else if mouse::in_rect(chunks[2], me.column, me.row) {
                self.history_scroll =
                    if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };

        if let Some(idx) = mouse::table_row_hit(x, y, chunks[0], 1, total, self.connections_tab.table_idx) {
            self.connections_tab.table_idx = idx;
            if idx < saved_len {
                if let Some(c) = self.cfg.connections.get(idx).cloned() {
                    self.connections_tab.load_from(idx, &c);
                }
            } else if let Some(s) = extras.get(idx - saved_len) {
                // Not a saved row, just prefills the form so Save becomes
                // a one-click "keep this one" instead of typing it all by
                // hand — the highlighted selection still follows it (via
                // `table_idx`) even though there's nothing in `cfg` yet.
                self.connections_tab.clear_form();
                self.connections_tab.fill_from_host(s);
            }
            return;
        }

        let form_inner = mouse::block_inner(chunks[1]);
        let rows2 = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // 0 Label
                Constraint::Length(1),
                Constraint::Length(1), // 2 Host
                Constraint::Length(1),
                Constraint::Length(1), // 4 SSH Port
                Constraint::Length(1),
                Constraint::Length(1), // 6 SSH User
                Constraint::Length(1),
                Constraint::Length(1), // 8 SSH Key Path
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1), // 11 SSH Password
                Constraint::Length(1),
                Constraint::Length(1), // 13 buttons
            ])
            .split(form_inner);

        if let Some(i) = mouse::button_row_hit(x, y, rows2[13], &["Save", "New", "Delete", "Test Connection"]) {
            match i {
                0 => self.trigger_save_connection(),
                1 => self.connections_tab.clear_form(),
                2 => self.trigger_delete_connection(),
                _ => self.trigger_test_connection(),
            }
            return;
        }

        if mouse::in_rect(rows2[0], x, y) {
            self.connections_tab.field = ConnField::Label;
        } else if mouse::in_rect(rows2[2], x, y) {
            self.connections_tab.host_picker = Some(HostPicker::new());
        } else if mouse::in_rect(rows2[4], x, y) {
            self.connections_tab.field = ConnField::SshPort;
        } else if mouse::in_rect(rows2[6], x, y) {
            self.connections_tab.field = ConnField::SshUser;
        } else if mouse::in_rect(rows2[8], x, y) {
            self.connections_tab.key_picker = Some(FilePicker::new(self.connections_tab.ssh_key_path.value()));
        } else if mouse::in_rect(rows2[11], x, y) {
            self.connections_tab.field = ConnField::SshPassword;
        }
    }

    fn handle_viewer_mouse(&mut self, me: MouseEvent, area: Rect) {
        if self.viewer_tab.browser.is_some() {
            self.handle_browser_mouse(me, area);
            return;
        }
        let vt = &self.viewer_tab;
        if vt.connection_dropdown_open {
            if mouse::left_click(&me).is_some() {
                self.viewer_tab.connection_dropdown_open = false;
            }
            return;
        }

        let (_, form_height) = viewer_form_constraints(vt.source);
        let outer =
            Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(form_height), Constraint::Min(10), Constraint::Length(6)]).split(area);

        if let Some(delta) = mouse::scroll_delta(&me) {
            if mouse::in_rect(outer[1], me.column, me.row) {
                let max_scroll = self.viewer_tab.log_lines.len() as u16;
                if delta < 0 {
                    self.viewer_tab.scroll = self.viewer_tab.scroll.saturating_sub(3);
                } else {
                    self.viewer_tab.scroll = (self.viewer_tab.scroll + 3).min(max_scroll);
                }
            } else if mouse::in_rect(outer[2], me.column, me.row) {
                self.history_scroll =
                    if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };

        if mouse::in_rect(outer[1], x, y) {
            self.viewer_tab.field = ViewerField::LogPane;
            return;
        }

        let form_inner = mouse::block_inner(outer[0]);
        let (constraints, _) = viewer_form_constraints(vt.source);
        let rows = Layout::default().direction(Direction::Vertical).margin(1).constraints(constraints).split(form_inner);

        let is_journal = vt.source == Source::Journal;
        let unit_row = 4;
        let (priority_row, lines_row, search_row, autorefresh_row, btn_row) =
            if is_journal { (8, 10, 12, 14, 16) } else { (6, 8, 10, 12, 14) };

        if mouse::in_rect(rows[0], x, y) {
            self.viewer_tab.field = ViewerField::Connection;
            self.viewer_tab.connection_idx = 0;
            self.viewer_tab.connection_dropdown_open = true;
            return;
        }
        if mouse::in_rect(rows[2], x, y) {
            self.viewer_tab.field = ViewerField::Source;
            self.viewer_tab.source = if is_journal { Source::File } else { Source::Journal };
            return;
        }
        if mouse::in_rect(rows[unit_row], x, y) {
            self.viewer_tab.field = ViewerField::UnitOrPath;
            return;
        }
        if is_journal && mouse::in_rect(rows[6], x, y) {
            self.viewer_tab.field = ViewerField::Since;
            return;
        }
        if mouse::in_rect(rows[priority_row], x, y) {
            self.viewer_tab.field = ViewerField::Priority;
            return;
        }
        if mouse::in_rect(rows[lines_row], x, y) {
            self.viewer_tab.field = ViewerField::Lines;
            return;
        }
        if mouse::in_rect(rows[search_row], x, y) {
            self.viewer_tab.field = ViewerField::Search;
            return;
        }
        if mouse::in_rect(rows[autorefresh_row], x, y) {
            self.viewer_tab.field = ViewerField::AutoRefresh;
            self.viewer_tab.auto_refresh = !self.viewer_tab.auto_refresh;
            return;
        }
        if !is_journal {
            if let Some(i) = mouse::button_row_hit(x, y, rows[btn_row], &["Browse", "Fetch"]) {
                if i == 0 {
                    self.trigger_browse();
                } else {
                    self.trigger_fetch();
                }
            }
        } else if mouse::button_row_hit(x, y, rows[btn_row], &["Fetch"]).is_some() {
            self.trigger_fetch();
        }
    }

    // ── Connections ──────────────────────────────────────────────────
    fn handle_connections_key(&mut self, key: KeyEvent) {
        let n = self.cfg.connections.len();
        // Computed up front, not where it's used below, because
        // `known_ssh_extras` needs `&self` as a whole, which the `ct`
        // borrow of just `self.connections_tab` would otherwise conflict
        // with for the rest of this function.
        let extras = self.known_ssh_extras();
        let total = n + extras.len();
        let ct = &mut self.connections_tab;

        if ct.key_picker.is_some() {
            match key.code {
                KeyCode::Esc => ct.key_picker = None,
                KeyCode::Up => {
                    if let Some(p) = ct.key_picker.as_mut() {
                        p.up();
                    }
                }
                KeyCode::Down => {
                    if let Some(p) = ct.key_picker.as_mut() {
                        p.down();
                    }
                }
                KeyCode::Enter => {
                    let picked = ct.key_picker.as_mut().and_then(|p| p.activate());
                    if let Some(path) = picked {
                        ct.ssh_key_path = Input::new(&path.to_string_lossy());
                        ct.key_picker = None;
                    }
                }
                _ => {}
            }
            return;
        }

        if ct.host_picker.is_some() {
            match key.code {
                KeyCode::Esc => ct.host_picker = None,
                KeyCode::Up => {
                    if let Some(p) = ct.host_picker.as_mut() {
                        p.up();
                    }
                }
                KeyCode::Down => {
                    if let Some(p) = ct.host_picker.as_mut() {
                        p.down();
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(p) = ct.host_picker.as_mut() {
                        p.insert(c);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(p) = ct.host_picker.as_mut() {
                        p.backspace();
                    }
                }
                KeyCode::Enter => {
                    let picked = ct.host_picker.as_ref().and_then(|p| p.activate());
                    if let Some(server) = picked {
                        ct.fill_from_host(&server);
                        ct.host_picker = None;
                    }
                }
                _ => {}
            }
            return;
        }

        // The table also lists SSH Server Manager hosts below the saved
        // profiles (see `known_ssh_extras`) — `total` spans both so
        // Up/Down (and therefore ratatui's own auto-scroll-to-selected)
        // can actually reach a host past whatever fits on screen, not
        // just the saved ones.
        if ct.field == ConnField::Table && total > 0 {
            match key.code {
                KeyCode::Up => {
                    if ct.table_idx > 0 {
                        ct.table_idx -= 1;
                    }
                    return;
                }
                KeyCode::Down => {
                    if ct.table_idx + 1 < total {
                        ct.table_idx += 1;
                    }
                    return;
                }
                KeyCode::Enter => {
                    let idx = ct.table_idx;
                    if idx < n {
                        if let Some(c) = self.cfg.connections.get(idx).cloned() {
                            ct.load_from(idx, &c);
                        }
                    } else if let Some(s) = extras.get(idx - n) {
                        ct.clear_form();
                        ct.fill_from_host(s);
                    }
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Tab => ct.next_field(),
            KeyCode::BackTab => ct.prev_field(),
            KeyCode::Up => ct.prev_field(),
            KeyCode::Down => ct.next_field(),
            KeyCode::Enter => match ct.field {
                ConnField::Host => ct.host_picker = Some(HostPicker::new()),
                ConnField::SshKeyPath => ct.key_picker = Some(FilePicker::new(ct.ssh_key_path.value())),
                ConnField::BtnSave => self.trigger_save_connection(),
                ConnField::BtnNew => self.connections_tab.clear_form(),
                ConnField::BtnDelete => self.trigger_delete_connection(),
                ConnField::BtnTest => self.trigger_test_connection(),
                _ => self.connections_tab.next_field(),
            },
            KeyCode::Char(c) => match ct.field {
                ConnField::Label => ct.label.insert(c),
                ConnField::Host => ct.host.insert(c),
                ConnField::SshPort => ct.ssh_port.insert(c),
                ConnField::SshUser => ct.ssh_user.insert(c),
                ConnField::SshKeyPath => ct.ssh_key_path.insert(c),
                ConnField::SshPassword => ct.ssh_password.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match ct.field {
                ConnField::Label => ct.label.backspace(),
                ConnField::Host => ct.host.backspace(),
                ConnField::SshPort => ct.ssh_port.backspace(),
                ConnField::SshUser => ct.ssh_user.backspace(),
                ConnField::SshKeyPath => ct.ssh_key_path.backspace(),
                ConnField::SshPassword => ct.ssh_password.backspace(),
                _ => {}
            },
            KeyCode::Delete => match ct.field {
                ConnField::Label => ct.label.delete(),
                ConnField::Host => ct.host.delete(),
                ConnField::SshPort => ct.ssh_port.delete(),
                ConnField::SshUser => ct.ssh_user.delete(),
                ConnField::SshKeyPath => ct.ssh_key_path.delete(),
                ConnField::SshPassword => ct.ssh_password.delete(),
                _ => {}
            },
            KeyCode::Left => match ct.field {
                ConnField::Label => ct.label.left(),
                ConnField::Host => ct.host.left(),
                ConnField::SshPort => ct.ssh_port.left(),
                ConnField::SshUser => ct.ssh_user.left(),
                ConnField::SshKeyPath => ct.ssh_key_path.left(),
                ConnField::SshPassword => ct.ssh_password.left(),
                _ => {}
            },
            KeyCode::Right => match ct.field {
                ConnField::Label => ct.label.right(),
                ConnField::Host => ct.host.right(),
                ConnField::SshPort => ct.ssh_port.right(),
                ConnField::SshUser => ct.ssh_user.right(),
                ConnField::SshKeyPath => ct.ssh_key_path.right(),
                ConnField::SshPassword => ct.ssh_password.right(),
                _ => {}
            },
            KeyCode::Home => match ct.field {
                ConnField::Label => ct.label.home(),
                ConnField::Host => ct.host.home(),
                ConnField::SshPort => ct.ssh_port.home(),
                ConnField::SshUser => ct.ssh_user.home(),
                ConnField::SshKeyPath => ct.ssh_key_path.home(),
                ConnField::SshPassword => ct.ssh_password.home(),
                _ => {}
            },
            KeyCode::End => match ct.field {
                ConnField::Label => ct.label.end_of_line(),
                ConnField::Host => ct.host.end_of_line(),
                ConnField::SshPort => ct.ssh_port.end_of_line(),
                ConnField::SshUser => ct.ssh_user.end_of_line(),
                ConnField::SshKeyPath => ct.ssh_key_path.end_of_line(),
                ConnField::SshPassword => ct.ssh_password.end_of_line(),
                _ => {}
            },
            _ => {}
        }
    }

    fn trigger_save_connection(&mut self) {
        let input = self.connections_tab.as_input();
        if input.label.is_empty() || input.host.is_empty() || input.ssh_user.is_empty() {
            self.error("Label, Host and SSH User are required");
            return;
        }
        let existing_id = self.connections_tab.selected.and_then(|i| self.cfg.connections.get(i)).map(|c| c.id.clone());
        match self.cfg.upsert_connection(existing_id.as_deref(), input) {
            Ok(_) => {
                if let Err(e) = config::save(&self.cfg) {
                    self.error(e.to_string());
                    return;
                }
                self.connections_tab.clear_form();
                self.history.push((true, "Connection saved".into()));
            }
            Err(e) => self.error(e.to_string()),
        }
    }

    fn trigger_delete_connection(&mut self) {
        let Some(idx) = self.connections_tab.selected.or_else(|| (!self.cfg.connections.is_empty()).then_some(self.connections_tab.table_idx))
        else {
            self.error("No connection selected");
            return;
        };
        let Some(c) = self.cfg.connections.get(idx).cloned() else { return };
        self.cfg.delete_connection(&c.id);
        match config::save(&self.cfg) {
            Ok(_) => {
                self.connections_tab.clear_form();
                self.connections_tab.table_idx = 0;
                self.history.push((true, format!("Connection '{}' deleted", c.label)));
            }
            Err(e) => self.error(e.to_string()),
        }
    }

    fn trigger_test_connection(&mut self) {
        let input = self.connections_tab.as_input();
        if input.host.is_empty() || input.ssh_user.is_empty() {
            self.error("Host and SSH User are required to test");
            return;
        }
        let ssh_password = if !input.ssh_password.is_empty() {
            input.ssh_password.clone()
        } else {
            self.connections_tab
                .selected
                .and_then(|i| self.cfg.connections.get(i))
                .and_then(|c| self.cfg.with_secrets(&c.id))
                .map(|c| c.ssh_password)
                .unwrap_or_default()
        };
        let cfg = ConnectionWithSecrets {
            id: "test".into(),
            label: input.label.clone(),
            host: input.host,
            ssh_port: input.ssh_port,
            ssh_user: input.ssh_user,
            ssh_key_path: input.ssh_key_path,
            ssh_password,
        };
        let tx = self.tx.clone();
        thread::spawn(move || match client::connect(&cfg) {
            Ok(_sess) => {
                let _ = tx.send(Msg::Log(true, "Connected OK".to_string()));
            }
            Err(e) => {
                let _ = tx.send(Msg::Log(false, format!("Connection failed: {}", one_line(&e))));
            }
        });
    }

    // ── Viewer ───────────────────────────────────────────────────────
    fn handle_viewer_key(&mut self, key: KeyEvent) {
        if self.viewer_tab.connection_dropdown_open {
            self.handle_connection_dropdown_key(key);
            return;
        }

        if self.viewer_tab.field == ViewerField::LogPane {
            let max_scroll = self.viewer_tab.log_lines.len() as u16;
            match key.code {
                KeyCode::Up => {
                    self.viewer_tab.scroll = self.viewer_tab.scroll.saturating_sub(1);
                    return;
                }
                KeyCode::Down => {
                    self.viewer_tab.scroll = (self.viewer_tab.scroll + 1).min(max_scroll);
                    return;
                }
                KeyCode::PageUp => {
                    self.viewer_tab.scroll = self.viewer_tab.scroll.saturating_sub(20);
                    return;
                }
                KeyCode::PageDown => {
                    self.viewer_tab.scroll = (self.viewer_tab.scroll + 20).min(max_scroll);
                    return;
                }
                KeyCode::Home => {
                    self.viewer_tab.scroll = 0;
                    return;
                }
                KeyCode::End => {
                    self.viewer_tab.scroll = max_scroll;
                    return;
                }
                _ => {}
            }
        }

        let vt = &mut self.viewer_tab;
        if vt.field == ViewerField::Source {
            match key.code {
                KeyCode::Left | KeyCode::Right => {
                    vt.source = if vt.source == Source::Journal { Source::File } else { Source::Journal };
                    return;
                }
                _ => {}
            }
        }
        if vt.field == ViewerField::Priority {
            match key.code {
                KeyCode::Left => {
                    let len = client::PRIORITIES.len();
                    vt.priority_idx = (vt.priority_idx + len - 1) % len;
                    return;
                }
                KeyCode::Right => {
                    vt.priority_idx = (vt.priority_idx + 1) % client::PRIORITIES.len();
                    return;
                }
                _ => {}
            }
        }
        if vt.field == ViewerField::AutoRefresh {
            match key.code {
                KeyCode::Left | KeyCode::Right => {
                    vt.auto_refresh = !vt.auto_refresh;
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Tab => vt.next_field(),
            KeyCode::BackTab => vt.prev_field(),
            KeyCode::Up => vt.prev_field(),
            KeyCode::Down => vt.next_field(),
            KeyCode::Enter => match vt.field {
                ViewerField::Connection => {
                    self.viewer_tab.connection_idx = 0;
                    self.viewer_tab.connection_dropdown_open = true;
                }
                ViewerField::Source => vt.source = if vt.source == Source::Journal { Source::File } else { Source::Journal },
                ViewerField::Priority => vt.priority_idx = (vt.priority_idx + 1) % client::PRIORITIES.len(),
                ViewerField::AutoRefresh => vt.auto_refresh = !vt.auto_refresh,
                ViewerField::BtnBrowse => self.trigger_browse(),
                ViewerField::BtnFetch => self.trigger_fetch(),
                _ => self.viewer_tab.next_field(),
            },
            KeyCode::Char(c) => match vt.field {
                ViewerField::Connection => {
                    vt.connection_input.insert(c);
                    vt.connection_dropdown_open = true;
                    vt.connection_idx = 0;
                }
                ViewerField::UnitOrPath if vt.source == Source::Journal => vt.unit.insert(c),
                ViewerField::UnitOrPath => vt.path.insert(c),
                ViewerField::Since => vt.since.insert(c),
                ViewerField::Lines => {
                    if c.is_ascii_digit() {
                        vt.lines.insert(c);
                    }
                }
                ViewerField::Search => vt.search.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match vt.field {
                ViewerField::Connection => vt.connection_input.backspace(),
                ViewerField::UnitOrPath if vt.source == Source::Journal => vt.unit.backspace(),
                ViewerField::UnitOrPath => vt.path.backspace(),
                ViewerField::Since => vt.since.backspace(),
                ViewerField::Lines => vt.lines.backspace(),
                ViewerField::Search => vt.search.backspace(),
                _ => {}
            },
            KeyCode::Delete => match vt.field {
                ViewerField::Connection => vt.connection_input.delete(),
                ViewerField::UnitOrPath if vt.source == Source::Journal => vt.unit.delete(),
                ViewerField::UnitOrPath => vt.path.delete(),
                ViewerField::Since => vt.since.delete(),
                ViewerField::Lines => vt.lines.delete(),
                ViewerField::Search => vt.search.delete(),
                _ => {}
            },
            KeyCode::Left => match vt.field {
                ViewerField::Connection => vt.connection_input.left(),
                ViewerField::UnitOrPath if vt.source == Source::Journal => vt.unit.left(),
                ViewerField::UnitOrPath => vt.path.left(),
                ViewerField::Since => vt.since.left(),
                ViewerField::Lines => vt.lines.left(),
                ViewerField::Search => vt.search.left(),
                _ => {}
            },
            KeyCode::Right => match vt.field {
                ViewerField::Connection => vt.connection_input.right(),
                ViewerField::UnitOrPath if vt.source == Source::Journal => vt.unit.right(),
                ViewerField::UnitOrPath => vt.path.right(),
                ViewerField::Since => vt.since.right(),
                ViewerField::Lines => vt.lines.right(),
                ViewerField::Search => vt.search.right(),
                _ => {}
            },
            KeyCode::Home => match vt.field {
                ViewerField::Connection => vt.connection_input.home(),
                ViewerField::UnitOrPath if vt.source == Source::Journal => vt.unit.home(),
                ViewerField::UnitOrPath => vt.path.home(),
                ViewerField::Since => vt.since.home(),
                ViewerField::Lines => vt.lines.home(),
                ViewerField::Search => vt.search.home(),
                _ => {}
            },
            KeyCode::End => match vt.field {
                ViewerField::Connection => vt.connection_input.end_of_line(),
                ViewerField::UnitOrPath if vt.source == Source::Journal => vt.unit.end_of_line(),
                ViewerField::UnitOrPath => vt.path.end_of_line(),
                ViewerField::Since => vt.since.end_of_line(),
                ViewerField::Lines => vt.lines.end_of_line(),
                ViewerField::Search => vt.search.end_of_line(),
                _ => {}
            },
            _ => {}
        }
    }

    fn handle_connection_dropdown_key(&mut self, key: KeyEvent) {
        let vt = &mut self.viewer_tab;
        let matches = filtered_connections(&self.cfg, vt.connection_input.value());
        let mut just_selected = false;
        match key.code {
            KeyCode::Esc => vt.connection_dropdown_open = false,
            KeyCode::Tab => {
                vt.connection_dropdown_open = false;
                vt.next_field();
            }
            KeyCode::BackTab => {
                vt.connection_dropdown_open = false;
                vt.prev_field();
            }
            KeyCode::Up => {
                if vt.connection_idx > 0 {
                    vt.connection_idx -= 1;
                }
            }
            KeyCode::Down => {
                if vt.connection_idx + 1 < matches.len() {
                    vt.connection_idx += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(label) = matches.get(vt.connection_idx) {
                    vt.connection_input = Input::new(label);
                    just_selected = true;
                }
                vt.connection_dropdown_open = false;
            }
            KeyCode::Char(c) => {
                vt.connection_input.insert(c);
                vt.connection_idx = 0;
            }
            KeyCode::Backspace => {
                vt.connection_input.backspace();
                vt.connection_idx = 0;
            }
            KeyCode::Delete => vt.connection_input.delete(),
            KeyCode::Left => vt.connection_input.left(),
            KeyCode::Right => vt.connection_input.right(),
            KeyCode::Home => vt.connection_input.home(),
            KeyCode::End => vt.connection_input.end_of_line(),
            _ => {}
        }
        // Picking a connection is the whole point of the dropdown — fetch
        // logs with it immediately instead of making the user hunt for a
        // separate Fetch button right after.
        if just_selected {
            self.trigger_fetch();
        }
    }

    fn trigger_fetch(&mut self) {
        let label = self.viewer_tab.connection_input.value().trim().to_string();
        let Some(cfg) = self.connection_by_label(&label) else {
            self.error("Select a valid connection (use the dropdown)");
            return;
        };
        let lines: u32 = self.viewer_tab.lines.value().trim().parse().unwrap_or(200).max(1);
        let params = client::FetchParams {
            source: self.viewer_tab.source,
            unit: self.viewer_tab.unit.value().trim().to_string(),
            since: self.viewer_tab.since.value().trim().to_string(),
            path: self.viewer_tab.path.value().trim().to_string(),
            priority: self.viewer_tab.priority(),
            search: self.viewer_tab.search.value().trim().to_string(),
            lines,
        };
        if params.source == Source::File && params.path.is_empty() {
            self.error("File Path is required in File mode");
            return;
        }
        self.viewer_tab.fetching = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = (|| -> Result<Vec<String>, String> {
                let sess = client::connect(&cfg)?;
                client::fetch_logs(&sess, &params)
            })();
            let _ = tx.send(Msg::FetchResult(result));
        });
    }

    fn trigger_browse(&mut self) {
        let label = self.viewer_tab.connection_input.value().trim().to_string();
        let Some(cfg) = self.connection_by_label(&label) else {
            self.error("Select a valid connection (use the dropdown)");
            return;
        };
        let start_path = {
            let p = self.viewer_tab.path.value().trim();
            if p.is_empty() {
                "/var/log".to_string()
            } else if p.ends_with('/') {
                p.to_string()
            } else {
                // Browsing a file path starts in its parent directory.
                client::join_remote_path(p, "..")
            }
        };
        self.viewer_tab.browser = Some(RemoteBrowser { path: start_path.clone(), entries: Vec::new(), selected: 0, loading: true, error: None });
        self.load_dir(cfg, start_path);
    }

    fn load_dir(&self, cfg: ConnectionWithSecrets, path: String) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = (|| -> Result<Vec<RemoteEntry>, String> {
                let sess = client::connect(&cfg)?;
                client::list_dir(&sess, &path)
            })();
            let _ = tx.send(Msg::BrowseResult(path, result));
        });
    }

    fn handle_browser_mouse(&mut self, me: MouseEvent, area: Rect) {
        let width = 84u16.min(area.width.saturating_sub(4));
        let height = 24u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);
        let inner = mouse::block_inner(modal_area);
        let rows = Layout::default().direction(Direction::Vertical).margin(1).constraints([Constraint::Min(3), Constraint::Length(1)]).split(inner);

        let Some((x, y)) = mouse::left_click(&me) else { return };
        let Some(browser) = &self.viewer_tab.browser else { return };
        // Matches draw_browser's combined list (a synthetic ".." row first
        // when not at "/", then every real entry) so a clicked row maps
        // to the same index draw() would highlight for it.
        let entry_count = browser.entries.len() + if browser.path != "/" { 1 } else { 0 };
        let Some(row) = mouse::plain_row_hit(x, y, rows[0], entry_count) else { return };

        if let Some(b) = &mut self.viewer_tab.browser {
            b.selected = row;
        }
        // Reuses the keyboard Enter handler instead of re-deriving
        // open-dir-vs-pick-file here, so mouse and keyboard always agree.
        self.handle_browser_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    fn handle_browser_key(&mut self, key: KeyEvent) {
        let Some(browser) = &mut self.viewer_tab.browser else { return };
        match key.code {
            KeyCode::Esc => {
                self.viewer_tab.browser = None;
            }
            KeyCode::Up => {
                if browser.selected > 0 {
                    browser.selected -= 1;
                }
            }
            KeyCode::Down => {
                if browser.selected + 1 < browser.entries.len() {
                    browser.selected += 1;
                }
            }
            KeyCode::Enter => {
                let Some(entry) = browser.entries.get(browser.selected) else { return };
                if entry.is_dir {
                    let next_path = client::join_remote_path(&browser.path, &entry.name);
                    let label = self.viewer_tab.connection_input.value().trim().to_string();
                    let Some(cfg) = self.connection_by_label(&label) else {
                        self.viewer_tab.browser = None;
                        self.error("Select a valid connection (use the dropdown)");
                        return;
                    };
                    if let Some(b) = &mut self.viewer_tab.browser {
                        b.loading = true;
                    }
                    self.load_dir(cfg, next_path);
                } else {
                    let full_path = client::join_remote_path(&browser.path, &entry.name);
                    self.viewer_tab.path = Input::new(&full_path);
                    self.viewer_tab.browser = None;
                }
            }
            _ => {}
        }
    }

    // ── Drawing ───────────────────────────────────────────────────────
    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(area);

        let tab_bar = Line::from(vec![
            tab_span("F1 Connections", self.tab == Tab::Connections),
            Span::styled("  ", Style::default().bg(bg())),
            tab_span("F2 Viewer", self.tab == Tab::Viewer),
            Span::styled("  ", Style::default().bg(bg())),
            Span::styled("Esc back  Ctrl+C quit", Style::default().fg(fg2()).bg(bg())),
        ]);
        f.render_widget(Paragraph::new(tab_bar).style(Style::default().bg(bg())), chunks[0]);

        match self.tab {
            Tab::Connections => self.draw_connections(f, chunks[1]),
            Tab::Viewer => self.draw_viewer(f, chunks[1]),
        }

        if let Some((title, msg)) = &self.modal {
            draw_modal(f, title, msg, area);
        }
    }

    fn draw_connections(&self, f: &mut Frame, area: Rect) {
        let ct = &self.connections_tab;
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(0), Constraint::Length(22), Constraint::Length(6)]).split(area);

        let header = Row::new(vec![
            Cell::from(Span::styled("Label", Style::default().fg(title_color()))),
            Cell::from(Span::styled("Host:Port", Style::default().fg(title_color()))),
            Cell::from(Span::styled("SSH User", Style::default().fg(title_color()))),
        ])
        .style(Style::default().bg(bg2()));
        let mut rows: Vec<Row> = self
            .cfg
            .connections
            .iter()
            .map(|c| Row::new(vec![Cell::from(c.label.clone()), Cell::from(format!("{}:{}", c.host, c.ssh_port)), Cell::from(c.ssh_user.clone())]))
            .collect();
        // Hosts from the SSH Server Manager show up here too, dimmed, so
        // they're visible and one click away from being usable without
        // ever going through "Add / Edit Connection" — clicking one just
        // fills the form below from it (see `handle_connections_mouse`),
        // the same as it does for a saved row.
        let extras = self.known_ssh_extras();
        for s in &extras {
            let port = if s.port == 0 { "22".to_string() } else { s.port.to_string() };
            rows.push(Row::new(vec![
                Cell::from(Span::styled(s.alias.clone(), Style::default().fg(fg2()))),
                Cell::from(Span::styled(format!("{}:{port}", s.effective_host()), Style::default().fg(fg2()))),
                Cell::from(Span::styled(s.user.clone(), Style::default().fg(fg2()))),
            ]));
        }
        let title = if extras.is_empty() {
            " Connections ".to_string()
        } else {
            format!(" Connections ({} saved + {} from ~/.ssh/config) ", self.cfg.connections.len(), extras.len())
        };
        let table = Table::new(rows, [Constraint::Length(20), Constraint::Length(26), Constraint::Length(16)])
            .header(header)
            .block(theme_block(&title))
            .row_highlight_style(if ct.field == ConnField::Table { focused() } else { normal() })
            .highlight_symbol(" \u{25B6} ")
            .style(Style::default().fg(fg()).bg(bg()));
        let total = self.cfg.connections.len() + extras.len();
        let mut tstate = TableState::default();
        if total > 0 {
            tstate.select(Some(ct.table_idx.min(total - 1)));
        }
        f.render_stateful_widget(table, chunks[0], &mut tstate);

        let form_block = theme_block(" Add / Edit Connection ");
        let form_inner = form_block.inner(chunks[1]);
        f.render_widget(form_block, chunks[1]);
        let rows2 = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // 0 Label
                Constraint::Length(1), // 1 spacer
                Constraint::Length(1), // 2 Host
                Constraint::Length(1), // 3 spacer
                Constraint::Length(1), // 4 SSH Port
                Constraint::Length(1), // 5 spacer
                Constraint::Length(1), // 6 SSH User
                Constraint::Length(1), // 7 spacer
                Constraint::Length(1), // 8 SSH Key Path
                Constraint::Length(1), // 9 hint
                Constraint::Length(1), // 10 spacer
                Constraint::Length(1), // 11 SSH Password
                Constraint::Length(1), // 12 spacer
                Constraint::Length(1), // 13 buttons
                Constraint::Length(1), // 14 spacer
                Constraint::Length(1), // 15 nav hint
                Constraint::Min(0),
            ])
            .split(form_inner);
        let fw = rows2[0].width.saturating_sub(16) as usize;

        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Label:          ", lbl()), input_span(&ct.label, ct.field == ConnField::Label, false, fw)])),
            rows2[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Host:           ", lbl()), input_span(&ct.host, ct.field == ConnField::Host, false, fw)])),
            rows2[2],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("SSH Port:       ", lbl()), input_span(&ct.ssh_port, ct.field == ConnField::SshPort, false, fw)])),
            rows2[4],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("SSH User:       ", lbl()), input_span(&ct.ssh_user, ct.field == ConnField::SshUser, false, fw)])),
            rows2[6],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("SSH Key Path:   ", lbl()),
                input_span(&ct.ssh_key_path, ct.field == ConnField::SshKeyPath, false, fw),
            ])),
            rows2[8],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("Enter opens a file picker; needs read access to system logs (root/adm/wheel/journal group)", lbl()))),
            rows2[9],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("SSH Password:   ", lbl()),
                input_span(&ct.ssh_password, ct.field == ConnField::SshPassword, true, fw),
            ])),
            rows2[11],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                btn_span("Save", ct.field == ConnField::BtnSave),
                Span::raw("  "),
                btn_span("New", ct.field == ConnField::BtnNew),
                Span::raw("  "),
                btn_span("Delete", ct.field == ConnField::BtnDelete),
                Span::raw("  "),
                btn_span("Test Connection", ct.field == ConnField::BtnTest),
            ])),
            rows2[13],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "\u{2191}\u{2193} select row  Enter on Host picks from SSH Server Manager  Tab navigate  Esc back",
                lbl(),
            ))),
            rows2[15],
        );

        draw_history(f, &self.history, chunks[2], self.history_scroll);

        if let Some(picker) = &ct.key_picker {
            super::file_picker::draw(f, picker, area);
        }
        if let Some(picker) = &ct.host_picker {
            super::host_picker::draw(f, picker, area);
        }
    }

    fn draw_viewer(&self, f: &mut Frame, area: Rect) {
        let vt = &self.viewer_tab;

        let (constraints, form_height) = viewer_form_constraints(vt.source);

        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(form_height), Constraint::Min(10), Constraint::Length(6)])
            .split(area);

        let form_block = theme_block(" Filters ");
        let form_inner = form_block.inner(outer[0]);
        f.render_widget(form_block, outer[0]);
        let rows = Layout::default().direction(Direction::Vertical).margin(1).constraints(constraints).split(form_inner);
        let fw = rows[0].width.saturating_sub(16) as usize;

        let mut pos = 0usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Connection:     ", lbl()),
                input_span(&vt.connection_input, vt.field == ViewerField::Connection, false, fw),
            ])),
            rows[pos],
        );
        pos += 2;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Source:         ", lbl()),
                btn_span(if vt.source == Source::Journal { "Journal (journalctl)" } else { "File (tail)" }, vt.field == ViewerField::Source),
                Span::styled("  (\u{2190}/\u{2192} to change)", lbl()),
            ])),
            rows[pos],
        );
        pos += 2;
        if vt.source == Source::Journal {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Unit (opt):     ", lbl()),
                    input_span(&vt.unit, vt.field == ViewerField::UnitOrPath, false, fw),
                ])),
                rows[pos],
            );
        } else {
            f.render_widget(
                Paragraph::new(Line::from(vec![Span::styled("File Path:      ", lbl()), input_span(&vt.path, vt.field == ViewerField::UnitOrPath, false, fw)])),
                rows[pos],
            );
        }
        pos += 2;
        if vt.source == Source::Journal {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Since (opt):    ", lbl()),
                    input_span(&vt.since, vt.field == ViewerField::Since, false, fw),
                ])),
                rows[pos],
            );
            pos += 2;
        }
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Priority:       ", lbl()),
                btn_span(vt.priority().label(), vt.field == ViewerField::Priority),
                Span::styled("  (\u{2190}/\u{2192} to change, shows this level and worse)", lbl()),
            ])),
            rows[pos],
        );
        pos += 2;
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Lines:          ", lbl()), input_span(&vt.lines, vt.field == ViewerField::Lines, false, fw)])),
            rows[pos],
        );
        pos += 2;
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Search:         ", lbl()), input_span(&vt.search, vt.field == ViewerField::Search, false, fw)])),
            rows[pos],
        );
        pos += 2;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Auto-refresh:   ", lbl()),
                btn_span(if vt.auto_refresh { "On" } else { "Off" }, vt.field == ViewerField::AutoRefresh),
                Span::styled(format!("  (\u{2190}/\u{2192} to change, every {AUTO_REFRESH_SECS}s)"), lbl()),
            ])),
            rows[pos],
        );
        pos += 2;
        let mut buttons = Vec::new();
        if vt.source == Source::File {
            buttons.push(btn_span("Browse", vt.field == ViewerField::BtnBrowse));
            buttons.push(Span::raw("  "));
        }
        buttons.push(btn_span(if vt.fetching { "Fetching\u{2026}" } else { "Fetch" }, vt.field == ViewerField::BtnFetch));
        f.render_widget(Paragraph::new(Line::from(buttons)), rows[pos]);
        pos += 2;
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("Tab navigate  \u{2022}  Enter activate  \u{2022}  \u{2191}\u{2193} on log pane scrolls  \u{2022}  Esc back", lbl()))),
            rows[pos],
        );

        let log_title = format!(" Log ({} line(s)) {} ", vt.log_lines.len(), if vt.field == ViewerField::LogPane { "\u{2014} \u{2191}\u{2193}/PgUp/PgDn/Home/End scroll" } else { "" });
        let log_block = theme_block(&log_title);
        let log_inner = log_block.inner(outer[1]);
        f.render_widget(log_block, outer[1]);
        let log_lines: Vec<Line> = vt.log_lines.iter().map(|l| colorize_line(l)).collect();
        f.render_widget(
            Paragraph::new(Text::from(log_lines)).wrap(Wrap { trim: false }).scroll((vt.scroll, 0)),
            log_inner,
        );

        draw_history(f, &self.history, outer[2], self.history_scroll);

        if vt.connection_dropdown_open {
            render_dropdown(f, &filtered_connections(&self.cfg, vt.connection_input.value()), vt.connection_idx, rows[0], 16, area);
        }

        if let Some(browser) = &vt.browser {
            draw_browser(f, browser, area);
        }
    }
}

fn colorize_line(line: &str) -> Line<'static> {
    let lower = line.to_lowercase();
    let style = if lower.contains("emerg") || lower.contains("alert") || lower.contains("crit") || lower.contains("fatal") || lower.contains(" error") || lower.contains("err:") || lower.contains("[error]") {
        Style::default().fg(red())
    } else if lower.contains("warn") {
        Style::default().fg(yellow())
    } else {
        Style::default().fg(fg())
    };
    Line::from(Span::styled(line.to_string(), style))
}

fn draw_browser(f: &mut Frame, browser: &RemoteBrowser, area: Rect) {
    let width = 84u16.min(area.width.saturating_sub(4));
    let height = 24u16.min(area.height.saturating_sub(2));
    let modal_area = centered_rect(width, height, area);
    f.render_widget(Clear, modal_area);
    let block = Block::default()
        .title(Span::styled(format!(" Browse — {} ", browser.path), Style::default().fg(title_color())))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent()))
        .style(Style::default().bg(bg2()));
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    let rows = Layout::default().direction(Direction::Vertical).margin(1).constraints([Constraint::Min(3), Constraint::Length(1)]).split(inner);

    if browser.loading {
        f.render_widget(Paragraph::new(Line::from(Span::styled("Loading\u{2026}", lbl()))), rows[0]);
    } else if let Some(err) = &browser.error {
        f.render_widget(Paragraph::new(Line::from(Span::styled(format!("can't list this directory: {err}"), Style::default().fg(red())))), rows[0]);
    } else {
        let mut items: Vec<ListItem> = Vec::new();
        if browser.path != "/" {
            items.push(ListItem::new(Span::styled("../", Style::default().fg(accent()))));
        }
        items.extend(browser.entries.iter().filter(|e| e.name != "..").map(|e| {
            let label = if e.is_dir { format!("{}/", e.name) } else { e.name.clone() };
            let style = if e.is_dir { Style::default().fg(accent()) } else { Style::default().fg(fg()) };
            ListItem::new(Span::styled(label, style))
        }));
        let list = List::new(items).highlight_style(focused()).style(Style::default().fg(fg()).bg(bg2()));
        let mut state = ListState::default();
        let entry_count = browser.entries.len() + if browser.path != "/" { 1 } else { 0 };
        if entry_count > 0 {
            state.select(Some(browser.selected.min(entry_count - 1)));
        }
        f.render_stateful_widget(list, rows[0], &mut state);
    }

    f.render_widget(
        Paragraph::new(Line::from(Span::styled("\u{2191}\u{2193} navigate  Enter open dir / pick file  Esc cancel", lbl()))),
        rows[1],
    );
}

/// Saved Logs profiles plus every SSH Server Manager host not already
/// covered by one, merged into a single pick list — see
/// `LogsScreen::known_ssh_extras` for why hosts don't need to be saved
/// here first.
fn filtered_connections(cfg: &config::Config, query: &str) -> Vec<String> {
    let q = query.to_lowercase();
    // (label, host) pairs — host is included in the match too, so typing
    // an IP finds a connection just as well as typing its name.
    let mut entries: Vec<(String, String)> = cfg.connections.iter().map(|c| (c.label.clone(), c.host.clone())).collect();
    if let Ok(servers) = crate::easyssh_mgr::config::list_servers("") {
        for s in servers {
            if !cfg.connections.iter().any(|c| c.host == s.effective_host()) && !entries.iter().any(|(l, _)| l == &s.alias) {
                let host = s.effective_host().to_string();
                entries.push((s.alias, host));
            }
        }
    }
    entries
        .into_iter()
        .filter(|(label, host)| q.is_empty() || label.to_lowercase().contains(&q) || host.to_lowercase().contains(&q))
        .map(|(label, _)| label)
        .collect()
}

fn render_dropdown(f: &mut Frame, items: &[String], selected_idx: usize, anchor: Rect, x_off: u16, bounds: Rect) {
    if items.is_empty() {
        return;
    }
    let list_items: Vec<ListItem> = items.iter().map(|s| ListItem::new(s.clone())).collect();
    let list = List::new(list_items)
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(border())))
        .highlight_style(Style::default().fg(bg()).bg(fg()))
        .style(Style::default().fg(fg()).bg(bg2()));
    let x = anchor.x + x_off;
    let y = anchor.y + 1;
    let height = (items.len() as u16 + 2).min(10);
    let content_width = items.iter().map(|s| s.chars().count()).max().unwrap_or(10) as u16 + 4;
    let width = content_width.max(20).min(bounds.width.saturating_sub(x));
    let dd_area = Rect::new(x, y, width, height);
    let mut state = ListState::default();
    state.select(Some(selected_idx));
    f.render_widget(Clear, dd_area);
    f.render_stateful_widget(list, dd_area, &mut state);
}

