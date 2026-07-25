//! ClickHouse User Manager screen — save reusable connection profiles,
//! either direct SQL against ClickHouse's HTTP interface (optionally
//! through an SSH tunnel) or the legacy SSH + `users.d/*.xml` route some
//! deployments still rely on, and manage users on the selected one.

use std::{sync::mpsc, thread};

use arboard::Clipboard;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState},
    Frame,
};

use crate::clickhouse::{
    client::{self, UserInfo},
    config::{self, Connection, ConnectionInput, ConnectionWithSecrets},
    make_ssh_creds, sql_client,
};
use crate::ssh_exec::{one_line, SshSession};

use super::file_picker::FilePicker;
use super::host_picker::HostPicker;
use super::mouse;
use super::widgets::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Connections,
    Users,
}

// ── Backend dispatch: SQL-over-HTTP vs SSH + users.d/*.xml ─────────────────
fn ch_list_users(cfg: &ConnectionWithSecrets) -> Result<Vec<UserInfo>, String> {
    if cfg.is_sql() {
        sql_client::list_users(cfg)
    } else {
        let sess = SshSession::connect(&cfg.ssh_host, &cfg.ssh_port, &make_ssh_creds(cfg))?;
        client::list_users(&sess)
    }
}

fn ch_create_user(cfg: &ConnectionWithSecrets, username: &str, profile: &str, ips: &[String], password: &str) -> Result<String, String> {
    if cfg.is_sql() {
        sql_client::create_user(cfg, username, profile, ips, password)
    } else {
        let sess = SshSession::connect(&cfg.ssh_host, &cfg.ssh_port, &make_ssh_creds(cfg))?;
        client::create_user(&sess, username, profile, ips, &cfg.tag_mode, password)
    }
}

fn ch_update_user(cfg: &ConnectionWithSecrets, username: &str, profile: &str, ips: &[String], new_password: &str) -> Result<(), String> {
    if cfg.is_sql() {
        sql_client::update_user(cfg, username, profile, ips, new_password)
    } else {
        let sess = SshSession::connect(&cfg.ssh_host, &cfg.ssh_port, &make_ssh_creds(cfg))?;
        client::update_user(&sess, username, profile, ips, &cfg.tag_mode, new_password)
    }
}

fn ch_delete_user(cfg: &ConnectionWithSecrets, username: &str) -> Result<(), String> {
    if cfg.is_sql() {
        sql_client::delete_user(cfg, username)
    } else {
        let sess = SshSession::connect(&cfg.ssh_host, &cfg.ssh_port, &make_ssh_creds(cfg))?;
        client::delete_user(&sess, username)
    }
}

fn profile_str(is_default: bool) -> &'static str {
    if is_default { "default" } else { "readonly" }
}

fn parse_ips(raw: &str) -> Vec<String> {
    raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

// ── Connections tab ──────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnField {
    Table,
    Label,
    Mode,
    Host,
    Port,
    DbUser,
    DbPassword,
    UseTunnel,
    SshHost,
    SshPort,
    SshUser,
    SshKeyPath,
    SshPassword,
    TagMode,
    BtnSave,
    BtnNew,
    BtnDelete,
    BtnTest,
}

fn conn_active_fields(mode_is_sql: bool, use_tunnel: bool) -> Vec<ConnField> {
    let mut v = vec![ConnField::Table, ConnField::Label, ConnField::Mode];
    if mode_is_sql {
        v.extend([ConnField::Host, ConnField::Port, ConnField::DbUser, ConnField::DbPassword, ConnField::UseTunnel]);
        if use_tunnel {
            v.extend([ConnField::SshHost, ConnField::SshPort, ConnField::SshUser, ConnField::SshKeyPath, ConnField::SshPassword]);
        }
    } else {
        v.extend([ConnField::SshHost, ConnField::SshPort, ConnField::SshUser, ConnField::SshKeyPath, ConnField::SshPassword, ConnField::TagMode]);
    }
    v.extend([ConnField::BtnSave, ConnField::BtnNew, ConnField::BtnDelete, ConnField::BtnTest]);
    v
}

/// The Connections form's row constraints and overall height — shared
/// between `draw_connections` (to render) and `handle_connections_mouse`
/// (to hit-test clicks), so the two branchy, mode-dependent row counts
/// can never drift apart the way two hand-copied `Layout::split` calls
/// could. Row indices for each field within the resulting split are
/// fixed given `(mode_is_sql, use_tunnel)` — see the comments in
/// `handle_connections_mouse` for the derived numbers.
fn conn_form_constraints(mode_is_sql: bool, use_tunnel: bool) -> (Vec<Constraint>, u16) {
    let mut constraints = vec![
        Constraint::Length(1), // Label
        Constraint::Length(1), // spacer
        Constraint::Length(1), // Mode
        Constraint::Length(1), // spacer
    ];
    if mode_is_sql {
        constraints.extend([
            Constraint::Length(1), // Host
            Constraint::Length(1), // spacer
            Constraint::Length(1), // Port
            Constraint::Length(1), // spacer
            Constraint::Length(1), // DB User
            Constraint::Length(1), // hint
            Constraint::Length(1), // spacer
            Constraint::Length(1), // DB Password
            Constraint::Length(1), // spacer
            Constraint::Length(1), // Use Tunnel
        ]);
        if use_tunnel {
            constraints.extend([
                Constraint::Length(1), // spacer
                Constraint::Length(1), // SSH Host
                Constraint::Length(1), // spacer
                Constraint::Length(1), // SSH Port
                Constraint::Length(1), // spacer
                Constraint::Length(1), // SSH User
                Constraint::Length(1), // spacer
                Constraint::Length(1), // SSH Key Path
                Constraint::Length(1), // spacer
                Constraint::Length(1), // SSH Password
            ]);
        }
    } else {
        constraints.extend([
            Constraint::Length(1), // SSH Host
            Constraint::Length(1), // spacer
            Constraint::Length(1), // SSH Port
            Constraint::Length(1), // spacer
            Constraint::Length(1), // SSH User
            Constraint::Length(1), // spacer
            Constraint::Length(1), // SSH Key Path
            Constraint::Length(1), // spacer
            Constraint::Length(1), // SSH Password
            Constraint::Length(1), // spacer
            Constraint::Length(1), // XML root tag
        ]);
    }
    constraints.extend([
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

struct ConnectionsTab {
    selected: Option<usize>,
    table_idx: usize,
    label: Input,
    mode_is_sql: bool,
    host: Input,
    port: Input,
    db_user: Input,
    db_password: Input,
    use_tunnel: bool,
    ssh_host: Input,
    ssh_port: Input,
    ssh_user: Input,
    ssh_key_path: Input,
    ssh_password: Input,
    tag_mode_idx: usize,
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
            mode_is_sql: true,
            host: Input::default(),
            port: Input::new("8123"),
            db_user: Input::default(),
            db_password: Input::default(),
            use_tunnel: false,
            ssh_host: Input::default(),
            ssh_port: Input::new("22"),
            ssh_user: Input::default(),
            ssh_key_path: Input::default(),
            ssh_password: Input::default(),
            tag_mode_idx: 0,
            field: ConnField::Table,
            key_picker: None,
            host_picker: None,
        }
    }

    fn next_field(&mut self) {
        let fields = conn_active_fields(self.mode_is_sql, self.use_tunnel);
        let idx = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = fields[(idx + 1) % fields.len()];
    }

    fn prev_field(&mut self) {
        let fields = conn_active_fields(self.mode_is_sql, self.use_tunnel);
        let idx = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = fields[(idx + fields.len() - 1) % fields.len()];
    }

    fn clear_form(&mut self) {
        self.selected = None;
        self.label = Input::default();
        self.mode_is_sql = true;
        self.host = Input::default();
        self.port = Input::new("8123");
        self.db_user = Input::default();
        self.db_password = Input::default();
        self.use_tunnel = false;
        self.ssh_host = Input::default();
        self.ssh_port = Input::new("22");
        self.ssh_user = Input::default();
        self.ssh_key_path = Input::default();
        self.ssh_password = Input::default();
        self.tag_mode_idx = 0;
        self.key_picker = None;
        self.host_picker = None;
    }

    fn load_from(&mut self, idx: usize, c: &Connection) {
        self.selected = Some(idx);
        self.label = Input::new(&c.label);
        self.mode_is_sql = c.mode == config::MODE_SQL;
        self.host = Input::new(&c.host);
        self.port = Input::new(&c.port);
        self.db_user = Input::new(&c.db_user);
        self.db_password = Input::default();
        self.use_tunnel = c.use_tunnel;
        self.ssh_host = Input::new(&c.ssh_host);
        self.ssh_port = Input::new(&c.ssh_port);
        self.ssh_user = Input::new(&c.ssh_user);
        self.ssh_key_path = Input::new(&c.ssh_key_path);
        self.ssh_password = Input::default();
        self.tag_mode_idx = config::TAG_MODES.iter().position(|m| *m == c.tag_mode).unwrap_or(0);
        self.key_picker = None;
        self.host_picker = None;
    }

    /// Fills the SSH side of the form (host/port/user/identity file) from a
    /// host already known to the SSH Server Manager. This is relevant in
    /// both modes — SQL-over-tunnel and legacy SSH+XML both talk to the
    /// same SSH fields — the database-specific fields are always left for
    /// the user to fill in separately.
    fn fill_ssh_from_host(&mut self, server: &crate::easyssh_mgr::config::Server) {
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

    fn as_input(&self) -> ConnectionInput {
        ConnectionInput {
            label: self.label.value().trim().to_string(),
            mode: if self.mode_is_sql { config::MODE_SQL } else { config::MODE_SSH_XML }.to_string(),
            host: self.host.value().trim().to_string(),
            port: self.port.value().trim().to_string(),
            db_user: self.db_user.value().trim().to_string(),
            db_password: self.db_password.value().to_string(),
            use_tunnel: self.use_tunnel,
            ssh_host: self.ssh_host.value().trim().to_string(),
            ssh_port: self.ssh_port.value().trim().to_string(),
            ssh_user: self.ssh_user.value().trim().to_string(),
            ssh_key_path: self.ssh_key_path.value().trim().to_string(),
            ssh_password: self.ssh_password.value().to_string(),
            tag_mode: config::TAG_MODES[self.tag_mode_idx].to_string(),
        }
    }
}

// ── Users tab ────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum UsersField {
    Connection,
    BtnConnect,
    Table,
    BtnAddUser,
}

enum UserModal {
    Add(AddUserModal),
    Edit(EditUserModal),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AddField {
    Username,
    Password,
    BtnGenerate,
    Profile,
    Ips,
    BtnCreate,
    BtnCancel,
}

struct AddUserModal {
    username: Input,
    password: Input,
    profile_is_default: bool,
    ips: Input,
    field: AddField,
}

impl AddUserModal {
    fn new() -> Self {
        Self {
            username: Input::default(),
            password: Input::default(),
            profile_is_default: true,
            ips: Input::default(),
            field: AddField::Username,
        }
    }
    fn next_field(&mut self) {
        self.field = match self.field {
            AddField::Username => AddField::Password,
            AddField::Password => AddField::BtnGenerate,
            AddField::BtnGenerate => AddField::Profile,
            AddField::Profile => AddField::Ips,
            AddField::Ips => AddField::BtnCreate,
            AddField::BtnCreate => AddField::BtnCancel,
            AddField::BtnCancel => AddField::Username,
        };
    }
    fn prev_field(&mut self) {
        self.field = match self.field {
            AddField::Username => AddField::BtnCancel,
            AddField::Password => AddField::Username,
            AddField::BtnGenerate => AddField::Password,
            AddField::Profile => AddField::BtnGenerate,
            AddField::Ips => AddField::Profile,
            AddField::BtnCreate => AddField::Ips,
            AddField::BtnCancel => AddField::BtnCreate,
        };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditField {
    Password,
    Profile,
    Ips,
    BtnSave,
    BtnDelete,
    BtnCancel,
}

struct EditUserModal {
    username: String,
    password: Input,
    profile_is_default: bool,
    ips: Input,
    field: EditField,
}

impl EditUserModal {
    fn new(u: &UserInfo) -> Self {
        Self {
            username: u.name.clone(),
            password: Input::default(),
            profile_is_default: u.profile != "readonly",
            ips: Input::new(&u.ips.join(", ")),
            field: EditField::Password,
        }
    }
    fn next_field(&mut self) {
        self.field = match self.field {
            EditField::Password => EditField::Profile,
            EditField::Profile => EditField::Ips,
            EditField::Ips => EditField::BtnSave,
            EditField::BtnSave => EditField::BtnDelete,
            EditField::BtnDelete => EditField::BtnCancel,
            EditField::BtnCancel => EditField::Password,
        };
    }
    fn prev_field(&mut self) {
        self.field = match self.field {
            EditField::Password => EditField::BtnCancel,
            EditField::Profile => EditField::Password,
            EditField::Ips => EditField::Profile,
            EditField::BtnSave => EditField::Ips,
            EditField::BtnDelete => EditField::BtnSave,
            EditField::BtnCancel => EditField::BtnDelete,
        };
    }
}

struct UsersTab {
    connection_input: Input,
    connection_dropdown_open: bool,
    connection_idx: usize,
    rows: Vec<UserInfo>,
    selected_row: usize,
    field: UsersField,
    modal: Option<UserModal>,
}

impl UsersTab {
    fn new() -> Self {
        Self {
            connection_input: Input::default(),
            connection_dropdown_open: false,
            connection_idx: 0,
            rows: Vec::new(),
            selected_row: 0,
            field: UsersField::Connection,
            modal: None,
        }
    }
    fn next_field(&mut self) {
        self.field = match self.field {
            UsersField::Connection => UsersField::BtnConnect,
            UsersField::BtnConnect => UsersField::Table,
            UsersField::Table => UsersField::BtnAddUser,
            UsersField::BtnAddUser => UsersField::Connection,
        };
    }
    fn prev_field(&mut self) {
        self.field = match self.field {
            UsersField::Connection => UsersField::BtnAddUser,
            UsersField::BtnConnect => UsersField::Connection,
            UsersField::Table => UsersField::BtnConnect,
            UsersField::BtnAddUser => UsersField::Table,
        };
    }
}

enum Msg {
    Log(bool, String),
    UsersResult(Result<Vec<UserInfo>, String>),
}

pub struct ClickHouseScreen {
    tab: Tab,
    connections_tab: ConnectionsTab,
    users_tab: UsersTab,
    modal: Option<(String, String)>,
    cfg: config::Config,
    history: Vec<(bool, String)>,
    /// Lines scrolled up from the newest entry in the History panel — see `widgets::draw_history`.
    history_scroll: u16,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
}

impl ClickHouseScreen {
    pub fn new() -> Self {
        let cfg = config::load().unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        Self {
            tab: Tab::Connections,
            connections_tab: ConnectionsTab::new(),
            users_tab: UsersTab::new(),
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
                Msg::UsersResult(Ok(rows)) => {
                    self.history.push((true, format!("Fetched {} user(s)", rows.len())));
                    self.users_tab.rows = rows;
                    self.users_tab.selected_row = 0;
                }
                Msg::UsersResult(Err(e)) => self.history.push((false, one_line(&e))),
            }
        }
    }

    fn connection_by_label(&self, label: &str) -> Option<ConnectionWithSecrets> {
        let c = self.cfg.connections.iter().find(|c| c.label == label)?;
        self.cfg.with_secrets(&c.id)
    }

    /// Surfaces an error both as a modal (so it's seen right away) and as
    /// a permanent History entry (so it's still there after the modal is
    /// dismissed, and visible from whichever tab the user is on since
    /// both tabs share `history`).
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

        let pickers_closed = self.connections_tab.key_picker.is_none() && self.connections_tab.host_picker.is_none();
        match key.code {
            KeyCode::Esc if !self.users_tab.connection_dropdown_open && self.users_tab.modal.is_none() && pickers_closed => {
                return true
            }
            KeyCode::F(1) if self.users_tab.modal.is_none() && pickers_closed => {
                self.tab = Tab::Connections;
                return false;
            }
            KeyCode::F(2) if self.users_tab.modal.is_none() && pickers_closed => {
                self.tab = Tab::Users;
                return false;
            }
            _ => {}
        }

        match self.tab {
            Tab::Connections => self.handle_connections_key(key),
            Tab::Users => self.handle_users_key(key),
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
        if self.users_tab.modal.is_some() {
            return;
        }
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(area);

        if let Some((x, y)) = mouse::left_click(&me) {
            if let Some(i) = mouse::label_row_hit(x, y, chunks[0], &["F1 Connections", "F2 Users"]) {
                self.tab = if i == 0 { Tab::Connections } else { Tab::Users };
                return;
            }
        }

        match self.tab {
            Tab::Connections => self.handle_connections_mouse(me, chunks[1]),
            Tab::Users => self.handle_users_mouse(me, chunks[1]),
        }
    }

    /// Row indices below are derived from `conn_form_constraints`'s exact
    /// row sequence for each `(mode_is_sql, use_tunnel)` combination — see
    /// its doc comment. They're literal numbers rather than a walked
    /// cursor because the two branches (SQL vs SSH+XML) don't share a
    /// common field order to walk generically.
    fn handle_connections_mouse(&mut self, me: MouseEvent, area: Rect) {
        let ct = &self.connections_tab;
        if let Some(picker) = &ct.host_picker {
            if let Some((x, y)) = mouse::left_click(&me) {
                if let Some(idx) = picker.row_at(area, x, y) {
                    self.connections_tab.host_picker.as_mut().unwrap().selected = idx;
                    if let Some(server) = self.connections_tab.host_picker.as_ref().unwrap().activate() {
                        self.connections_tab.fill_ssh_from_host(&server);
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

        let (_, form_height) = conn_form_constraints(ct.mode_is_sql, ct.use_tunnel);
        let chunks =
            Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(0), Constraint::Length(form_height), Constraint::Length(6)]).split(area);

        if let Some(delta) = mouse::scroll_delta(&me) {
            let n = self.cfg.connections.len();
            if n > 0 && mouse::in_rect(chunks[0], me.column, me.row) {
                if delta < 0 && self.connections_tab.table_idx > 0 {
                    self.connections_tab.table_idx -= 1;
                } else if delta > 0 && self.connections_tab.table_idx + 1 < n {
                    self.connections_tab.table_idx += 1;
                }
            } else if mouse::in_rect(chunks[2], me.column, me.row) {
                self.history_scroll =
                    if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };

        if let Some(idx) = mouse::table_row_hit(x, y, chunks[0], 1, self.cfg.connections.len(), self.connections_tab.table_idx) {
            self.connections_tab.table_idx = idx;
            if let Some(c) = self.cfg.connections.get(idx).cloned() {
                self.connections_tab.load_from(idx, &c);
            }
            return;
        }

        let form_inner = mouse::block_inner(chunks[1]);
        let (constraints, _) = conn_form_constraints(ct.mode_is_sql, ct.use_tunnel);
        let rows2 = Layout::default().direction(Direction::Vertical).margin(1).constraints(constraints).split(form_inner);

        let btn_row = if ct.mode_is_sql { if ct.use_tunnel { 25 } else { 15 } } else { 16 };
        if let Some(i) = mouse::button_row_hit(x, y, rows2[btn_row], &["Save", "New", "Delete", "Test Connection"]) {
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
            return;
        }
        if mouse::in_rect(rows2[2], x, y) {
            self.connections_tab.field = ConnField::Mode;
            self.connections_tab.mode_is_sql = !self.connections_tab.mode_is_sql;
            return;
        }

        if ct.mode_is_sql {
            let field_rows: &[(usize, ConnField)] = &[(4, ConnField::Host), (6, ConnField::Port), (8, ConnField::DbUser), (11, ConnField::DbPassword)];
            for (i, field) in field_rows {
                if mouse::in_rect(rows2[*i], x, y) {
                    self.connections_tab.field = *field;
                    return;
                }
            }
            if mouse::in_rect(rows2[13], x, y) {
                self.connections_tab.field = ConnField::UseTunnel;
                self.connections_tab.use_tunnel = !self.connections_tab.use_tunnel;
                return;
            }
            if ct.use_tunnel {
                let tunnel_rows: &[(usize, ConnField)] = &[
                    (15, ConnField::SshHost),
                    (17, ConnField::SshPort),
                    (19, ConnField::SshUser),
                    (21, ConnField::SshKeyPath),
                    (23, ConnField::SshPassword),
                ];
                for (i, field) in tunnel_rows {
                    if mouse::in_rect(rows2[*i], x, y) {
                        if *field == ConnField::SshHost {
                            self.connections_tab.host_picker = Some(HostPicker::new());
                        } else {
                            self.connections_tab.field = *field;
                        }
                        return;
                    }
                }
            }
        } else {
            let field_rows: &[(usize, ConnField)] =
                &[(6, ConnField::SshPort), (8, ConnField::SshUser), (10, ConnField::SshKeyPath), (12, ConnField::SshPassword)];
            if mouse::in_rect(rows2[4], x, y) {
                self.connections_tab.host_picker = Some(HostPicker::new());
                return;
            }
            for (i, field) in field_rows {
                if mouse::in_rect(rows2[*i], x, y) {
                    self.connections_tab.field = *field;
                    return;
                }
            }
            if mouse::in_rect(rows2[14], x, y) {
                self.connections_tab.field = ConnField::TagMode;
                self.connections_tab.tag_mode_idx = (self.connections_tab.tag_mode_idx + 1) % config::TAG_MODES.len();
            }
        }
    }

    fn handle_users_mouse(&mut self, me: MouseEvent, area: Rect) {
        if self.users_tab.connection_dropdown_open {
            self.users_tab.connection_dropdown_open = false;
            return;
        }
        let chunks =
            Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(5), Constraint::Min(6), Constraint::Length(6), Constraint::Length(7)]).split(area);

        if let Some(delta) = mouse::scroll_delta(&me) {
            let n = self.users_tab.rows.len();
            if n > 0 && mouse::in_rect(chunks[1], me.column, me.row) {
                if delta < 0 && self.users_tab.selected_row > 0 {
                    self.users_tab.selected_row -= 1;
                } else if delta > 0 && self.users_tab.selected_row + 1 < n {
                    self.users_tab.selected_row += 1;
                }
            } else if mouse::in_rect(chunks[3], me.column, me.row) {
                self.history_scroll =
                    if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };

        let top_inner = mouse::block_inner(chunks[0]);
        let top_rows = Layout::default().direction(Direction::Vertical).margin(1).constraints([Constraint::Length(1), Constraint::Length(1)]).split(top_inner);
        if mouse::in_rect(top_rows[0], x, y) {
            self.users_tab.field = UsersField::Connection;
            if !self.cfg.connections.is_empty() {
                self.users_tab.connection_idx = 0;
                self.users_tab.connection_dropdown_open = true;
            }
            return;
        }
        if mouse::button_row_hit(x, y, top_rows[1], &["Connect / Refresh"]).is_some() {
            self.trigger_fetch_users();
            return;
        }

        if let Some(idx) = mouse::table_row_hit(x, y, chunks[1], 1, self.users_tab.rows.len(), self.users_tab.selected_row) {
            self.users_tab.selected_row = idx;
            if let Some(u) = self.users_tab.rows.get(idx) {
                self.users_tab.modal = Some(UserModal::Edit(EditUserModal::new(u)));
            }
            return;
        }

        let action_inner = mouse::block_inner(chunks[2]);
        let action_rows = Layout::default().direction(Direction::Vertical).margin(1).constraints([Constraint::Length(1), Constraint::Length(1)]).split(action_inner);
        if mouse::button_row_hit(x, y, action_rows[0], &["Add User"]).is_some() {
            if self.connection_by_label(self.users_tab.connection_input.value().trim()).is_none() {
                self.error("Select a valid connection first (use the dropdown)");
            } else {
                self.users_tab.modal = Some(UserModal::Add(AddUserModal::new()));
            }
        }
    }

    // ── Connections ──────────────────────────────────────────────────
    fn handle_connections_key(&mut self, key: KeyEvent) {
        let n = self.cfg.connections.len();
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
                        ct.fill_ssh_from_host(&server);
                        ct.host_picker = None;
                    }
                }
                _ => {}
            }
            return;
        }

        if ct.field == ConnField::Table && n > 0 {
            match key.code {
                KeyCode::Up => {
                    if ct.table_idx > 0 {
                        ct.table_idx -= 1;
                    }
                    return;
                }
                KeyCode::Down => {
                    if ct.table_idx + 1 < n {
                        ct.table_idx += 1;
                    }
                    return;
                }
                KeyCode::Enter => {
                    let idx = ct.table_idx;
                    if let Some(c) = self.cfg.connections.get(idx).cloned() {
                        ct.load_from(idx, &c);
                    }
                    return;
                }
                _ => {}
            }
        }

        if ct.field == ConnField::Mode || ct.field == ConnField::UseTunnel || ct.field == ConnField::TagMode {
            match key.code {
                KeyCode::Left | KeyCode::Right => {
                    match ct.field {
                        ConnField::Mode => ct.mode_is_sql = !ct.mode_is_sql,
                        ConnField::UseTunnel => ct.use_tunnel = !ct.use_tunnel,
                        ConnField::TagMode => {
                            let len = config::TAG_MODES.len();
                            ct.tag_mode_idx = if key.code == KeyCode::Left {
                                (ct.tag_mode_idx + len - 1) % len
                            } else {
                                (ct.tag_mode_idx + 1) % len
                            };
                        }
                        _ => unreachable!(),
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
                ConnField::Mode => ct.mode_is_sql = !ct.mode_is_sql,
                ConnField::UseTunnel => ct.use_tunnel = !ct.use_tunnel,
                ConnField::TagMode => {
                    ct.tag_mode_idx = (ct.tag_mode_idx + 1) % config::TAG_MODES.len();
                }
                ConnField::SshHost => ct.host_picker = Some(HostPicker::new()),
                ConnField::SshKeyPath => {
                    ct.key_picker = Some(FilePicker::new(ct.ssh_key_path.value()));
                }
                ConnField::BtnSave => self.trigger_save_connection(),
                ConnField::BtnNew => self.connections_tab.clear_form(),
                ConnField::BtnDelete => self.trigger_delete_connection(),
                ConnField::BtnTest => self.trigger_test_connection(),
                _ => self.connections_tab.next_field(),
            },
            KeyCode::Char(c) => match ct.field {
                ConnField::Label => ct.label.insert(c),
                ConnField::Host => ct.host.insert(c),
                ConnField::Port => ct.port.insert(c),
                ConnField::DbUser => ct.db_user.insert(c),
                ConnField::DbPassword => ct.db_password.insert(c),
                ConnField::SshHost => ct.ssh_host.insert(c),
                ConnField::SshPort => ct.ssh_port.insert(c),
                ConnField::SshUser => ct.ssh_user.insert(c),
                ConnField::SshKeyPath => ct.ssh_key_path.insert(c),
                ConnField::SshPassword => ct.ssh_password.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match ct.field {
                ConnField::Label => ct.label.backspace(),
                ConnField::Host => ct.host.backspace(),
                ConnField::Port => ct.port.backspace(),
                ConnField::DbUser => ct.db_user.backspace(),
                ConnField::DbPassword => ct.db_password.backspace(),
                ConnField::SshHost => ct.ssh_host.backspace(),
                ConnField::SshPort => ct.ssh_port.backspace(),
                ConnField::SshUser => ct.ssh_user.backspace(),
                ConnField::SshKeyPath => ct.ssh_key_path.backspace(),
                ConnField::SshPassword => ct.ssh_password.backspace(),
                _ => {}
            },
            KeyCode::Delete => match ct.field {
                ConnField::Label => ct.label.delete(),
                ConnField::Host => ct.host.delete(),
                ConnField::Port => ct.port.delete(),
                ConnField::DbUser => ct.db_user.delete(),
                ConnField::DbPassword => ct.db_password.delete(),
                ConnField::SshHost => ct.ssh_host.delete(),
                ConnField::SshPort => ct.ssh_port.delete(),
                ConnField::SshUser => ct.ssh_user.delete(),
                ConnField::SshKeyPath => ct.ssh_key_path.delete(),
                ConnField::SshPassword => ct.ssh_password.delete(),
                _ => {}
            },
            KeyCode::Left => match ct.field {
                ConnField::Label => ct.label.left(),
                ConnField::Host => ct.host.left(),
                ConnField::Port => ct.port.left(),
                ConnField::DbUser => ct.db_user.left(),
                ConnField::DbPassword => ct.db_password.left(),
                ConnField::SshHost => ct.ssh_host.left(),
                ConnField::SshPort => ct.ssh_port.left(),
                ConnField::SshUser => ct.ssh_user.left(),
                ConnField::SshKeyPath => ct.ssh_key_path.left(),
                ConnField::SshPassword => ct.ssh_password.left(),
                _ => {}
            },
            KeyCode::Right => match ct.field {
                ConnField::Label => ct.label.right(),
                ConnField::Host => ct.host.right(),
                ConnField::Port => ct.port.right(),
                ConnField::DbUser => ct.db_user.right(),
                ConnField::DbPassword => ct.db_password.right(),
                ConnField::SshHost => ct.ssh_host.right(),
                ConnField::SshPort => ct.ssh_port.right(),
                ConnField::SshUser => ct.ssh_user.right(),
                ConnField::SshKeyPath => ct.ssh_key_path.right(),
                ConnField::SshPassword => ct.ssh_password.right(),
                _ => {}
            },
            KeyCode::Home => match ct.field {
                ConnField::Label => ct.label.home(),
                ConnField::Host => ct.host.home(),
                ConnField::Port => ct.port.home(),
                ConnField::DbUser => ct.db_user.home(),
                ConnField::DbPassword => ct.db_password.home(),
                ConnField::SshHost => ct.ssh_host.home(),
                ConnField::SshPort => ct.ssh_port.home(),
                ConnField::SshUser => ct.ssh_user.home(),
                ConnField::SshKeyPath => ct.ssh_key_path.home(),
                ConnField::SshPassword => ct.ssh_password.home(),
                _ => {}
            },
            KeyCode::End => match ct.field {
                ConnField::Label => ct.label.end_of_line(),
                ConnField::Host => ct.host.end_of_line(),
                ConnField::Port => ct.port.end_of_line(),
                ConnField::DbUser => ct.db_user.end_of_line(),
                ConnField::DbPassword => ct.db_password.end_of_line(),
                ConnField::SshHost => ct.ssh_host.end_of_line(),
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
        if input.label.is_empty() {
            self.error("Label is required");
            return;
        }
        if input.mode == config::MODE_SQL && (input.host.is_empty() || input.db_user.is_empty()) {
            self.error("Host and DB User are required for SQL mode");
            return;
        }
        if input.ssh_host.is_empty() && (input.mode == config::MODE_SSH_XML || input.use_tunnel) {
            self.error("SSH Host is required");
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
        if input.mode == config::MODE_SQL && (input.host.is_empty() || input.db_user.is_empty()) {
            self.error("Host and DB User are required to test");
            return;
        }
        if input.ssh_host.is_empty() && (input.mode == config::MODE_SSH_XML || input.use_tunnel) {
            self.error("SSH Host is required to test");
            return;
        }
        let db_password = if !input.db_password.is_empty() {
            input.db_password.clone()
        } else {
            self.connections_tab
                .selected
                .and_then(|i| self.cfg.connections.get(i))
                .and_then(|c| self.cfg.with_secrets(&c.id))
                .map(|c| c.db_password)
                .unwrap_or_default()
        };
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
            mode: input.mode,
            host: input.host,
            port: input.port,
            db_user: input.db_user,
            db_password,
            use_tunnel: input.use_tunnel,
            ssh_host: input.ssh_host,
            ssh_port: input.ssh_port,
            ssh_user: input.ssh_user,
            ssh_key_path: input.ssh_key_path,
            ssh_password,
            tag_mode: input.tag_mode,
        };
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Msg::UsersResult(ch_list_users(&cfg)));
        });
    }

    // ── Users ────────────────────────────────────────────────────────
    fn handle_users_key(&mut self, key: KeyEvent) {
        if self.users_tab.modal.is_some() {
            self.handle_user_modal_key(key);
            return;
        }
        if self.users_tab.connection_dropdown_open {
            self.handle_connection_dropdown_key(key);
            return;
        }
        if self.users_tab.field == UsersField::Table && !self.users_tab.rows.is_empty() {
            match key.code {
                KeyCode::Up => {
                    if self.users_tab.selected_row > 0 {
                        self.users_tab.selected_row -= 1;
                    }
                    return;
                }
                KeyCode::Down => {
                    if self.users_tab.selected_row + 1 < self.users_tab.rows.len() {
                        self.users_tab.selected_row += 1;
                    }
                    return;
                }
                KeyCode::Enter => {
                    if let Some(u) = self.users_tab.rows.get(self.users_tab.selected_row) {
                        self.users_tab.modal = Some(UserModal::Edit(EditUserModal::new(u)));
                    }
                    return;
                }
                _ => {}
            }
        }

        let ut = &mut self.users_tab;
        match key.code {
            KeyCode::Tab => ut.next_field(),
            KeyCode::BackTab => ut.prev_field(),
            KeyCode::Up => ut.prev_field(),
            KeyCode::Down => ut.next_field(),
            KeyCode::Enter => match ut.field {
                UsersField::Connection => {
                    if !self.cfg.connections.is_empty() {
                        self.users_tab.connection_idx = 0;
                        self.users_tab.connection_dropdown_open = true;
                    }
                }
                UsersField::BtnConnect => self.trigger_fetch_users(),
                UsersField::BtnAddUser => {
                    if self.connection_by_label(self.users_tab.connection_input.value().trim()).is_none() {
                        self.error("Select a valid connection first (use the dropdown)");
                    } else {
                        self.users_tab.modal = Some(UserModal::Add(AddUserModal::new()));
                    }
                }
                _ => self.users_tab.next_field(),
            },
            KeyCode::Char(c) if ut.field == UsersField::Connection => {
                ut.connection_input.insert(c);
                if !self.cfg.connections.is_empty() {
                    self.users_tab.connection_dropdown_open = true;
                    self.users_tab.connection_idx = 0;
                }
            }
            KeyCode::Backspace if ut.field == UsersField::Connection => ut.connection_input.backspace(),
            KeyCode::Delete if ut.field == UsersField::Connection => ut.connection_input.delete(),
            KeyCode::Left if ut.field == UsersField::Connection => ut.connection_input.left(),
            KeyCode::Right if ut.field == UsersField::Connection => ut.connection_input.right(),
            KeyCode::Home if ut.field == UsersField::Connection => ut.connection_input.home(),
            KeyCode::End if ut.field == UsersField::Connection => ut.connection_input.end_of_line(),
            _ => {}
        }
    }

    fn handle_connection_dropdown_key(&mut self, key: KeyEvent) {
        let ut = &mut self.users_tab;
        let matches = filtered_connections(&self.cfg, ut.connection_input.value());
        let mut just_selected = false;
        match key.code {
            KeyCode::Esc => ut.connection_dropdown_open = false,
            KeyCode::Tab => {
                ut.connection_dropdown_open = false;
                ut.next_field();
            }
            KeyCode::BackTab => {
                ut.connection_dropdown_open = false;
                ut.prev_field();
            }
            KeyCode::Up => {
                if ut.connection_idx > 0 {
                    ut.connection_idx -= 1;
                }
            }
            KeyCode::Down => {
                if ut.connection_idx + 1 < matches.len() {
                    ut.connection_idx += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(label) = matches.get(ut.connection_idx) {
                    ut.connection_input = Input::new(label);
                    just_selected = true;
                }
                ut.connection_dropdown_open = false;
            }
            KeyCode::Char(c) => {
                ut.connection_input.insert(c);
                ut.connection_idx = 0;
            }
            KeyCode::Backspace => {
                ut.connection_input.backspace();
                ut.connection_idx = 0;
            }
            KeyCode::Delete => ut.connection_input.delete(),
            KeyCode::Left => ut.connection_input.left(),
            KeyCode::Right => ut.connection_input.right(),
            KeyCode::Home => ut.connection_input.home(),
            KeyCode::End => ut.connection_input.end_of_line(),
            _ => {}
        }
        // Picking a connection is the whole point of the dropdown — fetch
        // its users immediately instead of making the user hunt for a
        // separate "Connect" button.
        if just_selected {
            self.trigger_fetch_users();
        }
    }

    fn trigger_fetch_users(&mut self) {
        let label = self.users_tab.connection_input.value().trim().to_string();
        let Some(cfg) = self.connection_by_label(&label) else {
            self.error("Select a valid connection (use the dropdown)");
            return;
        };
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Msg::UsersResult(ch_list_users(&cfg)));
        });
    }

    fn handle_user_modal_key(&mut self, key: KeyEvent) {
        match self.users_tab.modal.take() {
            Some(UserModal::Add(mut m)) => {
                let close = self.handle_add_user_modal_key(&mut m, key);
                if !close {
                    self.users_tab.modal = Some(UserModal::Add(m));
                }
            }
            Some(UserModal::Edit(mut m)) => {
                let close = self.handle_edit_user_modal_key(&mut m, key);
                if !close {
                    self.users_tab.modal = Some(UserModal::Edit(m));
                }
            }
            None => {}
        }
    }

    fn handle_add_user_modal_key(&mut self, m: &mut AddUserModal, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => return true,
            KeyCode::Tab => m.next_field(),
            KeyCode::BackTab => m.prev_field(),
            KeyCode::Up => m.prev_field(),
            KeyCode::Down => m.next_field(),
            KeyCode::Left if m.field == AddField::Profile => m.profile_is_default = !m.profile_is_default,
            KeyCode::Right if m.field == AddField::Profile => m.profile_is_default = !m.profile_is_default,
            KeyCode::Enter => match m.field {
                AddField::Profile => m.profile_is_default = !m.profile_is_default,
                AddField::BtnCreate => {
                    let username = m.username.value().trim().to_string();
                    let password = m.password.value().to_string();
                    if username.is_empty() {
                        self.error("Username is required");
                        return false;
                    }
                    let profile = profile_str(m.profile_is_default).to_string();
                    let ips = parse_ips(m.ips.value());
                    let label = self.users_tab.connection_input.value().trim().to_string();
                    let Some(cfg) = self.connection_by_label(&label) else {
                        self.error("Select a valid connection");
                        return false;
                    };
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        match ch_create_user(&cfg, &username, &profile, &ips, &password) {
                            Ok(password) => {
                                let copied = Clipboard::new().and_then(|mut c| c.set_text(password.clone())).is_ok();
                                let suffix = if copied { " (copied to clipboard)" } else { "" };
                                let _ = tx.send(Msg::Log(true, format!("User {username} created, password: {password}{suffix}")));
                            }
                            Err(e) => {
                                let _ = tx.send(Msg::Log(false, format!("Failed to create {username}: {}", one_line(&e))));
                            }
                        }
                        let _ = tx.send(Msg::UsersResult(ch_list_users(&cfg)));
                    });
                    return true;
                }
                AddField::BtnCancel => return true,
                AddField::BtnGenerate => {
                    let pw = generate_password();
                    m.password = Input::new(&pw);
                    let copied = Clipboard::new().and_then(|mut c| c.set_text(pw.clone())).is_ok();
                    let suffix = if copied { " (copied to clipboard)" } else { "" };
                    self.history.push((true, format!("Generated password: {pw}{suffix}")));
                }
                _ => m.next_field(),
            },
            KeyCode::Char(c) => match m.field {
                AddField::Username => m.username.insert(c),
                AddField::Password => m.password.insert(c),
                AddField::Ips => m.ips.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match m.field {
                AddField::Username => m.username.backspace(),
                AddField::Password => m.password.backspace(),
                AddField::Ips => m.ips.backspace(),
                _ => {}
            },
            KeyCode::Delete => match m.field {
                AddField::Username => m.username.delete(),
                AddField::Password => m.password.delete(),
                AddField::Ips => m.ips.delete(),
                _ => {}
            },
            KeyCode::Left => match m.field {
                AddField::Username => m.username.left(),
                AddField::Password => m.password.left(),
                AddField::Ips => m.ips.left(),
                _ => {}
            },
            KeyCode::Right => match m.field {
                AddField::Username => m.username.right(),
                AddField::Password => m.password.right(),
                AddField::Ips => m.ips.right(),
                _ => {}
            },
            KeyCode::Home => match m.field {
                AddField::Username => m.username.home(),
                AddField::Password => m.password.home(),
                AddField::Ips => m.ips.home(),
                _ => {}
            },
            KeyCode::End => match m.field {
                AddField::Username => m.username.end_of_line(),
                AddField::Password => m.password.end_of_line(),
                AddField::Ips => m.ips.end_of_line(),
                _ => {}
            },
            _ => {}
        }
        false
    }

    fn handle_edit_user_modal_key(&mut self, m: &mut EditUserModal, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => return true,
            KeyCode::Tab => m.next_field(),
            KeyCode::BackTab => m.prev_field(),
            KeyCode::Up => m.prev_field(),
            KeyCode::Down => m.next_field(),
            KeyCode::Left if m.field == EditField::Profile => m.profile_is_default = !m.profile_is_default,
            KeyCode::Right if m.field == EditField::Profile => m.profile_is_default = !m.profile_is_default,
            KeyCode::Enter => match m.field {
                EditField::Profile => m.profile_is_default = !m.profile_is_default,
                EditField::BtnSave => {
                    let label = self.users_tab.connection_input.value().trim().to_string();
                    let Some(cfg) = self.connection_by_label(&label) else {
                        self.error("Select a valid connection");
                        return false;
                    };
                    let username = m.username.clone();
                    let profile = profile_str(m.profile_is_default).to_string();
                    let ips = parse_ips(m.ips.value());
                    let new_password = m.password.value().to_string();
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        match ch_update_user(&cfg, &username, &profile, &ips, &new_password) {
                            Ok(()) => {
                                let _ = tx.send(Msg::Log(true, format!("User {username} updated")));
                            }
                            Err(e) => {
                                let _ = tx.send(Msg::Log(false, format!("Failed to update {username}: {}", one_line(&e))));
                            }
                        }
                        let _ = tx.send(Msg::UsersResult(ch_list_users(&cfg)));
                    });
                    return true;
                }
                EditField::BtnDelete => {
                    let label = self.users_tab.connection_input.value().trim().to_string();
                    let Some(cfg) = self.connection_by_label(&label) else {
                        self.error("Select a valid connection");
                        return false;
                    };
                    let username = m.username.clone();
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        match ch_delete_user(&cfg, &username) {
                            Ok(()) => {
                                let _ = tx.send(Msg::Log(true, format!("User {username} deleted")));
                            }
                            Err(e) => {
                                let _ = tx.send(Msg::Log(false, format!("Failed to delete {username}: {}", one_line(&e))));
                            }
                        }
                        let _ = tx.send(Msg::UsersResult(ch_list_users(&cfg)));
                    });
                    return true;
                }
                EditField::BtnCancel => return true,
                _ => m.next_field(),
            },
            KeyCode::Char(c) => match m.field {
                EditField::Password => m.password.insert(c),
                EditField::Ips => m.ips.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match m.field {
                EditField::Password => m.password.backspace(),
                EditField::Ips => m.ips.backspace(),
                _ => {}
            },
            KeyCode::Delete => match m.field {
                EditField::Password => m.password.delete(),
                EditField::Ips => m.ips.delete(),
                _ => {}
            },
            KeyCode::Left => match m.field {
                EditField::Password => m.password.left(),
                EditField::Ips => m.ips.left(),
                _ => {}
            },
            KeyCode::Right => match m.field {
                EditField::Password => m.password.right(),
                EditField::Ips => m.ips.right(),
                _ => {}
            },
            KeyCode::Home => match m.field {
                EditField::Password => m.password.home(),
                EditField::Ips => m.ips.home(),
                _ => {}
            },
            KeyCode::End => match m.field {
                EditField::Password => m.password.end_of_line(),
                EditField::Ips => m.ips.end_of_line(),
                _ => {}
            },
            _ => {}
        }
        false
    }

    // ── Drawing ───────────────────────────────────────────────────────
    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(area);

        let tab_bar = Line::from(vec![
            tab_span("F1 Connections", self.tab == Tab::Connections),
            Span::styled("  ", Style::default().bg(bg())),
            tab_span("F2 Users", self.tab == Tab::Users),
            Span::styled("  ", Style::default().bg(bg())),
            Span::styled("Esc back  Ctrl+C quit", Style::default().fg(fg2()).bg(bg())),
        ]);
        f.render_widget(Paragraph::new(tab_bar).style(Style::default().bg(bg())), chunks[0]);

        match self.tab {
            Tab::Connections => self.draw_connections(f, chunks[1]),
            Tab::Users => self.draw_users(f, chunks[1]),
        }

        if self.tab == Tab::Users {
            match &self.users_tab.modal {
                Some(UserModal::Add(m)) => self.draw_add_user_modal(f, m, area),
                Some(UserModal::Edit(m)) => self.draw_edit_user_modal(f, m, area),
                None => {}
            }
        }

        if let Some((title, msg)) = &self.modal {
            draw_modal(f, title, msg, area);
        }
    }

    fn draw_connections(&self, f: &mut Frame, area: Rect) {
        let ct = &self.connections_tab;

        let (constraints, form_height) = conn_form_constraints(ct.mode_is_sql, ct.use_tunnel);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(form_height), Constraint::Length(6)])
            .split(area);

        let header = Row::new(vec![
            Cell::from(Span::styled("Label", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Mode", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Target", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Tunnel", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
        ])
        .style(Style::default().bg(bg2()));

        let rows: Vec<Row> = self
            .cfg
            .connections
            .iter()
            .map(|c| {
                let is_sql = c.mode == config::MODE_SQL;
                let target = if is_sql { format!("{}:{}", c.host, c.port) } else { format!("{} (ssh)", c.ssh_host) };
                let tunnel = if is_sql { if c.use_tunnel { "yes" } else { "no" } } else { "-" };
                Row::new(vec![
                    Cell::from(c.label.clone()),
                    Cell::from(if is_sql { "SQL" } else { "SSH XML" }),
                    Cell::from(target),
                    Cell::from(tunnel),
                ])
            })
            .collect();

        let table = Table::new(rows, [Constraint::Length(18), Constraint::Length(10), Constraint::Length(28), Constraint::Length(8)])
            .header(header)
            .block(theme_block(" Connections "))
            .row_highlight_style(if ct.field == ConnField::Table { focused() } else { normal() })
            .highlight_symbol(" \u{25B6} ")
            .style(Style::default().fg(fg()).bg(bg()));

        let mut tstate = TableState::default();
        if !self.cfg.connections.is_empty() {
            tstate.select(Some(ct.table_idx.min(self.cfg.connections.len() - 1)));
        }
        f.render_stateful_widget(table, chunks[0], &mut tstate);

        let form_block = theme_block(" Add / Edit Connection ");
        let form_inner = form_block.inner(chunks[1]);
        f.render_widget(form_block, chunks[1]);

        let rows2 = Layout::default().direction(Direction::Vertical).margin(1).constraints(constraints).split(form_inner);
        let fw = rows2[0].width.saturating_sub(16) as usize;

        let mut pos = 0usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Label:          ", lbl()), input_span(&ct.label, ct.field == ConnField::Label, false, fw)])),
            rows2[pos],
        );
        pos += 2;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Mode:           ", lbl()),
                btn_span(if ct.mode_is_sql { "SQL" } else { "SSH (XML)" }, ct.field == ConnField::Mode),
                Span::styled("  (\u{2190}/\u{2192} to change)", lbl()),
            ])),
            rows2[pos],
        );
        pos += 2;

        if ct.mode_is_sql {
            f.render_widget(
                Paragraph::new(Line::from(vec![Span::styled("Host:           ", lbl()), input_span(&ct.host, ct.field == ConnField::Host, false, fw)])),
                rows2[pos],
            );
            pos += 2;
            f.render_widget(
                Paragraph::new(Line::from(vec![Span::styled("Port:           ", lbl()), input_span(&ct.port, ct.field == ConnField::Port, false, fw)])),
                rows2[pos],
            );
            pos += 2;
            f.render_widget(
                Paragraph::new(Line::from(vec![Span::styled("DB User:        ", lbl()), input_span(&ct.db_user, ct.field == ConnField::DbUser, false, fw)])),
                rows2[pos],
            );
            pos += 1;
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "needs CREATE/ALTER/DROP USER privileges — e.g. the default admin user",
                    Style::default().fg(yellow()),
                ))),
                rows2[pos],
            );
            pos += 2;
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("DB Password:    ", lbl()),
                    input_span(&ct.db_password, ct.field == ConnField::DbPassword, true, fw),
                ])),
                rows2[pos],
            );
            pos += 2;
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Use SSH Tunnel: ", lbl()),
                    btn_span(if ct.use_tunnel { "Yes" } else { "No" }, ct.field == ConnField::UseTunnel),
                    Span::styled("  (\u{2190}/\u{2192} to change)", lbl()),
                ])),
                rows2[pos],
            );
            pos += 1;
            if ct.use_tunnel {
                pos += 1;
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("SSH Host:       ", lbl()),
                        input_span(&ct.ssh_host, ct.field == ConnField::SshHost, false, fw),
                    ])),
                    rows2[pos],
                );
                pos += 2;
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("SSH Port:       ", lbl()),
                        input_span(&ct.ssh_port, ct.field == ConnField::SshPort, false, fw),
                    ])),
                    rows2[pos],
                );
                pos += 2;
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("SSH User:       ", lbl()),
                        input_span(&ct.ssh_user, ct.field == ConnField::SshUser, false, fw),
                    ])),
                    rows2[pos],
                );
                pos += 2;
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("SSH Key Path:   ", lbl()),
                        input_span(&ct.ssh_key_path, ct.field == ConnField::SshKeyPath, false, fw),
                    ])),
                    rows2[pos],
                );
                pos += 2;
                f.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled("SSH Password:   ", lbl()),
                        input_span(&ct.ssh_password, ct.field == ConnField::SshPassword, true, fw),
                    ])),
                    rows2[pos],
                );
                pos += 1;
            }
        } else {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("SSH Host:       ", lbl()),
                    input_span(&ct.ssh_host, ct.field == ConnField::SshHost, false, fw),
                ])),
                rows2[pos],
            );
            pos += 2;
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("SSH Port:       ", lbl()),
                    input_span(&ct.ssh_port, ct.field == ConnField::SshPort, false, fw),
                ])),
                rows2[pos],
            );
            pos += 2;
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("SSH User:       ", lbl()),
                    input_span(&ct.ssh_user, ct.field == ConnField::SshUser, false, fw),
                ])),
                rows2[pos],
            );
            pos += 2;
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("SSH Key Path:   ", lbl()),
                    input_span(&ct.ssh_key_path, ct.field == ConnField::SshKeyPath, false, fw),
                ])),
                rows2[pos],
            );
            pos += 2;
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("SSH Password:   ", lbl()),
                    input_span(&ct.ssh_password, ct.field == ConnField::SshPassword, true, fw),
                ])),
                rows2[pos],
            );
            pos += 2;
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("XML root tag:   ", lbl()),
                    btn_span(config::TAG_MODES[ct.tag_mode_idx], ct.field == ConnField::TagMode),
                    Span::styled("  (\u{2190}/\u{2192} to change)", lbl()),
                ])),
                rows2[pos],
            );
            pos += 1;
        }

        pos += 1;
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
            rows2[pos],
        );
        pos += 2;
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "\u{2191}\u{2193} select row  Enter on SSH Host picks a known host  Tab navigate  Esc back",
                lbl(),
            ))),
            rows2[pos],
        );

        draw_history(f, &self.history, chunks[2], self.history_scroll);

        if let Some(picker) = &ct.key_picker {
            super::file_picker::draw(f, picker, area);
        }
        if let Some(picker) = &ct.host_picker {
            super::host_picker::draw(f, picker, area);
        }
    }

    fn draw_users(&self, f: &mut Frame, area: Rect) {
        let ut = &self.users_tab;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(6), Constraint::Length(6), Constraint::Length(7)])
            .split(area);

        let top_block = theme_block(" Connection ");
        let top_inner = top_block.inner(chunks[0]);
        f.render_widget(top_block, chunks[0]);
        let top_rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(top_inner);
        let fw = top_rows[0].width.saturating_sub(24) as usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Connection:             ", lbl()),
                input_span(&ut.connection_input, ut.field == UsersField::Connection, false, fw),
            ])),
            top_rows[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![btn_span("Connect / Refresh", ut.field == UsersField::BtnConnect)])),
            top_rows[1],
        );

        let header = Row::new(vec![
            Cell::from(Span::styled("Username", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Profile", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Allowed IPs", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
        ])
        .style(Style::default().bg(bg2()));
        let table_rows: Vec<Row> = ut
            .rows
            .iter()
            .map(|u| {
                let ips = if u.ips.is_empty() { "localhost only".to_string() } else { u.ips.join(", ") };
                Row::new(vec![Cell::from(u.name.clone()), Cell::from(u.profile.clone()), Cell::from(ips)])
            })
            .collect();
        let users_title = format!(" Users ({}) \u{2014} \u{2191}\u{2193} select \u{2022} Enter edit ", ut.rows.len());
        let table = Table::new(table_rows, [Constraint::Length(20), Constraint::Length(14), Constraint::Min(0)])
            .header(header)
            .block(theme_block(&users_title))
            .row_highlight_style(if ut.field == UsersField::Table { focused() } else { normal() })
            .highlight_symbol(" \u{25B6} ")
            .style(Style::default().fg(fg()).bg(bg()));
        let mut tstate = TableState::default();
        if !ut.rows.is_empty() {
            tstate.select(Some(ut.selected_row.min(ut.rows.len() - 1)));
        }
        f.render_stateful_widget(table, chunks[1], &mut tstate);

        let action_block = theme_block(" Actions ");
        let action_inner = action_block.inner(chunks[2]);
        f.render_widget(action_block, chunks[2]);
        let action_rows = Layout::default().direction(Direction::Vertical).margin(1).constraints([Constraint::Length(1), Constraint::Length(1)]).split(action_inner);
        f.render_widget(Paragraph::new(Line::from(vec![btn_span("Add User", ut.field == UsersField::BtnAddUser)])), action_rows[0]);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("Enter on a table row opens edit (password / profile / IPs / delete)", lbl()))),
            action_rows[1],
        );

        draw_history(f, &self.history, chunks[3], self.history_scroll);

        if ut.connection_dropdown_open {
            render_dropdown(f, &filtered_connections(&self.cfg, ut.connection_input.value()), ut.connection_idx, top_rows[0], 24, area);
        }
    }

    fn draw_add_user_modal(&self, f: &mut Frame, m: &AddUserModal, area: Rect) {
        let width = 70u16.min(area.width.saturating_sub(4));
        let height = 16u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);
        f.render_widget(Clear, modal_area);
        let block = Block::default()
            .title(Span::styled(" Add ClickHouse User ", Style::default().fg(title_color())))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(accent()))
            .style(Style::default().bg(bg2()));
        let inner = block.inner(modal_area);
        f.render_widget(block, modal_area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // [0] Username
                Constraint::Length(1), // [1] spacer
                Constraint::Length(1), // [2] Password
                Constraint::Length(1), // [3] spacer
                Constraint::Length(1), // [4] Profile
                Constraint::Length(1), // [5] spacer
                Constraint::Length(1), // [6] Allowed IPs
                Constraint::Length(1), // [7] hint
                Constraint::Length(1), // [8] spacer
                Constraint::Length(1), // [9] buttons
                Constraint::Length(1), // [10] spacer
                Constraint::Length(1), // [11] nav hint
                Constraint::Min(0),
            ])
            .split(inner);
        let fw = rows[0].width.saturating_sub(12) as usize;

        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Username: ", lbl()), input_span(&m.username, m.field == AddField::Username, false, fw)])),
            rows[0],
        );
        let pw_fw = fw.saturating_sub(14);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Password: ", lbl()),
                input_span(&m.password, m.field == AddField::Password, true, pw_fw),
                Span::raw(" "),
                btn_span("Generate", m.field == AddField::BtnGenerate),
            ])),
            rows[2],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Profile:  ", lbl()),
                btn_span(profile_str(m.profile_is_default), m.field == AddField::Profile),
                Span::styled("  (\u{2190}/\u{2192} to change)", lbl()),
            ])),
            rows[4],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Allowed IPs: ", lbl()), input_span(&m.ips, m.field == AddField::Ips, false, fw)])),
            rows[6],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "comma-separated, e.g. 192.168.0.0/16; empty = localhost only. leave Password empty to auto-generate",
                lbl(),
            ))),
            rows[7],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                btn_span("Create", m.field == AddField::BtnCreate),
                Span::raw("  "),
                btn_span("Cancel", m.field == AddField::BtnCancel),
            ])),
            rows[9],
        );
        f.render_widget(Paragraph::new(Line::from(Span::styled("Tab navigate  \u{2022}  Enter activate  \u{2022}  Esc cancel", lbl()))), rows[11]);
    }

    fn draw_edit_user_modal(&self, f: &mut Frame, m: &EditUserModal, area: Rect) {
        let width = 70u16.min(area.width.saturating_sub(4));
        let height = 16u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);
        f.render_widget(Clear, modal_area);
        let block = Block::default()
            .title(Span::styled(format!(" User \"{}\" ", m.username), Style::default().fg(title_color())))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(accent()))
            .style(Style::default().bg(bg2()));
        let inner = block.inner(modal_area);
        f.render_widget(block, modal_area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // [0] New Password
                Constraint::Length(1), // [1] hint
                Constraint::Length(1), // [2] spacer
                Constraint::Length(1), // [3] Profile
                Constraint::Length(1), // [4] spacer
                Constraint::Length(1), // [5] Allowed IPs
                Constraint::Length(1), // [6] hint
                Constraint::Length(1), // [7] spacer
                Constraint::Length(1), // [8] buttons
                Constraint::Length(1), // [9] spacer
                Constraint::Length(1), // [10] nav hint
                Constraint::Min(0),
            ])
            .split(inner);
        let fw = rows[0].width.saturating_sub(14) as usize;

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("New Password: ", lbl()),
                input_span(&m.password, m.field == EditField::Password, true, fw),
            ])),
            rows[0],
        );
        f.render_widget(Paragraph::new(Line::from(Span::styled("leave empty to keep the current password", lbl()))), rows[1]);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Profile:      ", lbl()),
                btn_span(profile_str(m.profile_is_default), m.field == EditField::Profile),
                Span::styled("  (\u{2190}/\u{2192} to change)", lbl()),
            ])),
            rows[3],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Allowed IPs:  ", lbl()),
                input_span(&m.ips, m.field == EditField::Ips, false, fw),
            ])),
            rows[5],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("comma-separated, e.g. 192.168.0.0/16; empty = localhost only", lbl()))),
            rows[6],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                btn_span("Save", m.field == EditField::BtnSave),
                Span::raw("  "),
                btn_span("Delete", m.field == EditField::BtnDelete),
                Span::raw("  "),
                btn_span("Cancel", m.field == EditField::BtnCancel),
            ])),
            rows[8],
        );
        f.render_widget(Paragraph::new(Line::from(Span::styled("Tab navigate  \u{2022}  Enter activate  \u{2022}  Esc cancel", lbl()))), rows[10]);
    }
}

/// Matches by label, or by whichever host field is actually the target
/// for that connection's mode (`host` for SQL, `ssh_host` for SSH+XML —
/// see `Connection::host`'s doc comment), so typing an IP finds it just
/// as well as typing the connection's name.
fn filtered_connections(cfg: &config::Config, query: &str) -> Vec<String> {
    let q = query.to_lowercase();
    cfg.connections
        .iter()
        .filter(|c| {
            if q.is_empty() {
                return true;
            }
            let target = if c.mode == config::MODE_SQL { &c.host } else { &c.ssh_host };
            c.label.to_lowercase().contains(&q) || target.to_lowercase().contains(&q)
        })
        .map(|c| c.label.clone())
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
    // Wide enough for the longest label instead of a fixed 30 cols,
    // which was cutting off longer connection names mid-character.
    let content_width = items.iter().map(|s| s.chars().count()).max().unwrap_or(10) as u16 + 4;
    let width = content_width.max(20).min(bounds.width.saturating_sub(x));
    let dd_area = Rect::new(x, y, width, height);
    let mut state = ListState::default();
    state.select(Some(selected_idx));
    f.render_widget(Clear, dd_area);
    f.render_stateful_widget(list, dd_area, &mut state);
}

