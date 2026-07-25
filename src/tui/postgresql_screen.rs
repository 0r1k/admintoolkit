//! PostgreSQL User Manager screen — save reusable connection profiles
//! (direct or via an SSH jump host) and manage roles on the selected one.

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

use crate::postgres_mgr::{
    client::{self, PgRole},
    config::{self, Connection, ConnectionInput, ConnectionWithSecrets},
};

use super::file_picker::FilePicker;
use super::host_picker::HostPicker;
use super::mouse;
use super::priv_picker::{self, PrivPicker};
use super::widgets::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Connections,
    Users,
}

// ── Connections tab ──────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnField {
    Table,
    Label,
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
    BtnSave,
    BtnNew,
    BtnDelete,
    BtnTest,
}

fn conn_active_fields(use_tunnel: bool) -> Vec<ConnField> {
    let mut v = vec![
        ConnField::Table,
        ConnField::Label,
        ConnField::Host,
        ConnField::Port,
        ConnField::DbUser,
        ConnField::DbPassword,
        ConnField::UseTunnel,
    ];
    if use_tunnel {
        v.extend([
            ConnField::SshHost,
            ConnField::SshPort,
            ConnField::SshUser,
            ConnField::SshKeyPath,
            ConnField::SshPassword,
        ]);
    }
    v.extend([ConnField::BtnSave, ConnField::BtnNew, ConnField::BtnDelete, ConnField::BtnTest]);
    v
}

/// The Connections form's row layout — shared between `draw_connections`
/// (to render) and `handle_connections_mouse` (to hit-test clicks) so the
/// two can never drift apart the way two independently-hand-copied
/// `Layout::split` calls could.
fn conn_form_rows(form_inner: Rect, use_tunnel: bool) -> Vec<Rect> {
    let mut constraints = vec![
        Constraint::Length(1), // [0] Label
        Constraint::Length(1), // [1] spacer
        Constraint::Length(1), // [2] Host
        Constraint::Length(1), // [3] spacer
        Constraint::Length(1), // [4] Port
        Constraint::Length(1), // [5] spacer
        Constraint::Length(1), // [6] DB User
        Constraint::Length(1), // [7] hint
        Constraint::Length(1), // [8] spacer
        Constraint::Length(1), // [9] DB Password
        Constraint::Length(1), // [10] spacer
        Constraint::Length(1), // [11] Use Tunnel
    ];
    if use_tunnel {
        constraints.extend([
            Constraint::Length(1), // [12] spacer
            Constraint::Length(1), // [13] SSH Host
            Constraint::Length(1), // [14] spacer
            Constraint::Length(1), // [15] SSH Port
            Constraint::Length(1), // [16] spacer
            Constraint::Length(1), // [17] SSH User
            Constraint::Length(1), // [18] spacer
            Constraint::Length(1), // [19] SSH Key Path
            Constraint::Length(1), // [20] spacer
            Constraint::Length(1), // [21] SSH Password
        ]);
    }
    constraints.extend([
        Constraint::Length(1), // spacer
        Constraint::Length(1), // buttons
        Constraint::Length(1), // spacer
        Constraint::Length(1), // nav hint
        Constraint::Min(0),
    ]);
    Layout::default().direction(Direction::Vertical).margin(1).constraints(constraints).split(form_inner).to_vec()
}

struct ConnectionsTab {
    selected: Option<usize>,
    table_idx: usize,
    label: Input,
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
            port: Input::new("5432"),
            db_user: Input::default(),
            db_password: Input::default(),
            use_tunnel: false,
            ssh_host: Input::default(),
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
        let fields = conn_active_fields(self.use_tunnel);
        let idx = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = fields[(idx + 1) % fields.len()];
    }

    fn prev_field(&mut self) {
        let fields = conn_active_fields(self.use_tunnel);
        let idx = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = fields[(idx + fields.len() - 1) % fields.len()];
    }

    fn clear_form(&mut self) {
        self.selected = None;
        self.label = Input::default();
        self.host = Input::default();
        self.port = Input::new("5432");
        self.db_user = Input::default();
        self.db_password = Input::default();
        self.use_tunnel = false;
        self.ssh_host = Input::default();
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
        self.port = Input::new(&c.port);
        self.db_user = Input::new(&c.db_user);
        self.db_password = Input::default();
        self.use_tunnel = c.use_tunnel;
        self.ssh_host = Input::new(&c.ssh_host);
        self.ssh_port = Input::new(&c.ssh_port);
        self.ssh_user = Input::new(&c.ssh_user);
        self.ssh_key_path = Input::new(&c.ssh_key_path);
        self.ssh_password = Input::default();
        self.key_picker = None;
        self.host_picker = None;
    }

    /// Fills the SSH-tunnel side of the form from a host already known to
    /// the SSH Server Manager. The DB-specific fields (DB host/port/user/
    /// password) are a different thing entirely — a tunnel's SSH jump
    /// host isn't the database itself — so those are always left for the
    /// user to fill in separately.
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
    GrantDb,
    Privileges,
    BtnCreate,
    BtnCancel,
}

struct AddUserModal {
    username: Input,
    password: Input,
    grant_db: Input,
    privileges: PrivPicker,
    field: AddField,
}

impl AddUserModal {
    fn new() -> Self {
        Self {
            username: Input::default(),
            password: Input::default(),
            grant_db: Input::default(),
            privileges: PrivPicker::new(client::PRIVILEGES),
            field: AddField::Username,
        }
    }
    fn next_field(&mut self) {
        self.field = match self.field {
            AddField::Username => AddField::Password,
            AddField::Password => AddField::BtnGenerate,
            AddField::BtnGenerate => AddField::GrantDb,
            AddField::GrantDb => AddField::Privileges,
            AddField::Privileges => AddField::BtnCreate,
            AddField::BtnCreate => AddField::BtnCancel,
            AddField::BtnCancel => AddField::Username,
        };
    }
    fn prev_field(&mut self) {
        self.field = match self.field {
            AddField::Username => AddField::BtnCancel,
            AddField::Password => AddField::Username,
            AddField::BtnGenerate => AddField::Password,
            AddField::GrantDb => AddField::BtnGenerate,
            AddField::Privileges => AddField::GrantDb,
            AddField::BtnCreate => AddField::Privileges,
            AddField::BtnCancel => AddField::BtnCreate,
        };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditField {
    Password,
    BtnChangePassword,
    BtnDrop,
    BtnCancel,
}

struct EditUserModal {
    user: String,
    password: Input,
    field: EditField,
}

impl EditUserModal {
    fn new(u: &PgRole) -> Self {
        Self {
            user: u.name.clone(),
            password: Input::default(),
            field: EditField::Password,
        }
    }
    fn next_field(&mut self) {
        self.field = match self.field {
            EditField::Password => EditField::BtnChangePassword,
            EditField::BtnChangePassword => EditField::BtnDrop,
            EditField::BtnDrop => EditField::BtnCancel,
            EditField::BtnCancel => EditField::Password,
        };
    }
    fn prev_field(&mut self) {
        self.field = match self.field {
            EditField::Password => EditField::BtnCancel,
            EditField::BtnChangePassword => EditField::Password,
            EditField::BtnDrop => EditField::BtnChangePassword,
            EditField::BtnCancel => EditField::BtnDrop,
        };
    }
}

struct UsersTab {
    connection_input: Input,
    connection_dropdown_open: bool,
    connection_idx: usize,
    rows: Vec<PgRole>,
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
    UsersResult(Result<Vec<PgRole>, String>),
}

pub struct PostgresqlScreen {
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

impl PostgresqlScreen {
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

        let form_height = if ct.use_tunnel { 30 } else { 20 };
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
        let rows2 = conn_form_rows(form_inner, ct.use_tunnel);

        if let Some(i) = mouse::button_row_hit(x, y, rows2[if ct.use_tunnel { 23 } else { 13 }], &["Save", "New", "Delete", "Test Connection"]) {
            match i {
                0 => self.trigger_save_connection(),
                1 => self.connections_tab.clear_form(),
                2 => self.trigger_delete_connection(),
                _ => self.trigger_test_connection(),
            }
            return;
        }

        if mouse::in_rect(rows2[11], x, y) {
            self.connections_tab.field = ConnField::UseTunnel;
            self.connections_tab.use_tunnel = !self.connections_tab.use_tunnel;
            return;
        }

        let field_rows: &[(usize, ConnField)] = &[(0, ConnField::Label), (2, ConnField::Host), (4, ConnField::Port), (6, ConnField::DbUser), (9, ConnField::DbPassword)];
        for (i, field) in field_rows {
            if mouse::in_rect(rows2[*i], x, y) {
                self.connections_tab.field = *field;
                return;
            }
        }
        if ct.use_tunnel {
            let tunnel_rows: &[(usize, ConnField)] = &[
                (13, ConnField::SshHost),
                (15, ConnField::SshPort),
                (17, ConnField::SshUser),
                (19, ConnField::SshKeyPath),
                (21, ConnField::SshPassword),
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

        if ct.field == ConnField::UseTunnel {
            match key.code {
                KeyCode::Left | KeyCode::Right => {
                    ct.use_tunnel = !ct.use_tunnel;
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
                ConnField::UseTunnel => ct.use_tunnel = !ct.use_tunnel,
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
        if input.label.is_empty() || input.host.is_empty() || input.db_user.is_empty() {
            self.error("Label, Host and DB User are required");
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
        if input.host.is_empty() || input.db_user.is_empty() {
            self.error("Host and DB User are required to test");
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
        };
        let tx = self.tx.clone();
        thread::spawn(move || match client::connect(&cfg) {
            Ok(mut conn) => {
                let rows = client::list_users(&mut conn).unwrap_or_default();
                let _ = tx.send(Msg::UsersResult(Ok(rows)));
            }
            Err(e) => {
                let _ = tx.send(Msg::Log(false, format!("Connection failed: {}", one_line(&e))));
            }
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
            let result = (|| -> Result<Vec<PgRole>, String> {
                let mut conn = client::connect(&cfg)?;
                client::list_users(&mut conn)
            })();
            let _ = tx.send(Msg::UsersResult(result));
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
            KeyCode::Enter => match m.field {
                AddField::BtnCreate => {
                    let username = m.username.value().trim().to_string();
                    let password = m.password.value().to_string();
                    if username.is_empty() || password.is_empty() {
                        self.error("Username and Password are required");
                        return false;
                    }
                    let grant_db = {
                        let g = m.grant_db.value().trim().to_string();
                        if g.is_empty() {
                            None
                        } else {
                            Some(g)
                        }
                    };
                    let privileges = m.privileges.selected_items();
                    let label = self.users_tab.connection_input.value().trim().to_string();
                    let Some(cfg) = self.connection_by_label(&label) else {
                        self.error("Select a valid connection");
                        return false;
                    };
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        let result = (|| -> Result<(), String> {
                            let mut conn = client::connect(&cfg)?;
                            client::create_user(&mut conn, &username, &password)?;
                            if let Some(db) = &grant_db {
                                client::grant_table_privileges(&cfg, db, &username, &privileges)?;
                            }
                            Ok(())
                        })();
                        match result {
                            Ok(()) => {
                                let _ = tx.send(Msg::Log(true, format!("User {username} created")));
                            }
                            Err(e) => {
                                let _ = tx.send(Msg::Log(false, format!("Failed to create {username}: {}", one_line(&e))));
                            }
                        }
                        let result2 = (|| -> Result<Vec<PgRole>, String> {
                            let mut conn = client::connect(&cfg)?;
                            client::list_users(&mut conn)
                        })();
                        let _ = tx.send(Msg::UsersResult(result2));
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
                AddField::Privileges => m.privileges.toggle(),
                _ => m.next_field(),
            },
            KeyCode::Char(' ') if m.field == AddField::Privileges => m.privileges.toggle(),
            KeyCode::Char(c) => match m.field {
                AddField::Username => m.username.insert(c),
                AddField::Password => m.password.insert(c),
                AddField::GrantDb => m.grant_db.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match m.field {
                AddField::Username => m.username.backspace(),
                AddField::Password => m.password.backspace(),
                AddField::GrantDb => m.grant_db.backspace(),
                _ => {}
            },
            KeyCode::Delete => match m.field {
                AddField::Username => m.username.delete(),
                AddField::Password => m.password.delete(),
                AddField::GrantDb => m.grant_db.delete(),
                _ => {}
            },
            KeyCode::Left => match m.field {
                AddField::Username => m.username.left(),
                AddField::Password => m.password.left(),
                AddField::GrantDb => m.grant_db.left(),
                AddField::Privileges => m.privileges.left(),
                _ => {}
            },
            KeyCode::Right => match m.field {
                AddField::Username => m.username.right(),
                AddField::Password => m.password.right(),
                AddField::GrantDb => m.grant_db.right(),
                AddField::Privileges => m.privileges.right(),
                _ => {}
            },
            KeyCode::Home => match m.field {
                AddField::Username => m.username.home(),
                AddField::Password => m.password.home(),
                AddField::GrantDb => m.grant_db.home(),
                _ => {}
            },
            KeyCode::End => match m.field {
                AddField::Username => m.username.end_of_line(),
                AddField::Password => m.password.end_of_line(),
                AddField::GrantDb => m.grant_db.end_of_line(),
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
            KeyCode::Enter => match m.field {
                EditField::BtnChangePassword => {
                    let new_password = m.password.value().to_string();
                    if new_password.is_empty() {
                        self.error("Enter a new password first");
                        return false;
                    }
                    let label = self.users_tab.connection_input.value().trim().to_string();
                    let Some(cfg) = self.connection_by_label(&label) else {
                        self.error("Select a valid connection");
                        return false;
                    };
                    let user = m.user.clone();
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        let result = (|| -> Result<(), String> {
                            let mut conn = client::connect(&cfg)?;
                            client::change_password(&mut conn, &user, &new_password)
                        })();
                        match result {
                            Ok(()) => {
                                let _ = tx.send(Msg::Log(true, format!("Password changed for {user}")));
                            }
                            Err(e) => {
                                let _ = tx.send(Msg::Log(false, format!("Failed to change password for {user}: {}", one_line(&e))));
                            }
                        }
                    });
                    return true;
                }
                EditField::BtnDrop => {
                    let label = self.users_tab.connection_input.value().trim().to_string();
                    let Some(cfg) = self.connection_by_label(&label) else {
                        self.error("Select a valid connection");
                        return false;
                    };
                    let user = m.user.clone();
                    let tx = self.tx.clone();
                    thread::spawn(move || {
                        let result = (|| -> Result<(), String> {
                            let mut conn = client::connect(&cfg)?;
                            client::drop_user(&mut conn, &user)
                        })();
                        match result {
                            Ok(()) => {
                                let _ = tx.send(Msg::Log(true, format!("User {user} dropped")));
                            }
                            Err(e) => {
                                let _ = tx.send(Msg::Log(false, format!("Failed to drop {user}: {}", one_line(&e))));
                            }
                        }
                        let result2 = (|| -> Result<Vec<PgRole>, String> {
                            let mut conn = client::connect(&cfg)?;
                            client::list_users(&mut conn)
                        })();
                        let _ = tx.send(Msg::UsersResult(result2));
                    });
                    return true;
                }
                EditField::BtnCancel => return true,
                _ => m.next_field(),
            },
            KeyCode::Char(c) if m.field == EditField::Password => m.password.insert(c),
            KeyCode::Backspace if m.field == EditField::Password => m.password.backspace(),
            KeyCode::Delete if m.field == EditField::Password => m.password.delete(),
            KeyCode::Left if m.field == EditField::Password => m.password.left(),
            KeyCode::Right if m.field == EditField::Password => m.password.right(),
            KeyCode::Home if m.field == EditField::Password => m.password.home(),
            KeyCode::End if m.field == EditField::Password => m.password.end_of_line(),
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
        // Content rows: 16 fixed (Label..UseTunnel + spacer/buttons/hint) plus,
        // when the tunnel is on, 10 more for the SSH fields — plus border(2)
        // and the inner margin(2). Undersizing this collapses rows to zero
        // height instead of just being short on room, so it must track
        // `use_tunnel` exactly rather than use one fixed guess.
        let form_height = if ct.use_tunnel { 30 } else { 20 };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(form_height), Constraint::Length(6)])
            .split(area);

        let header = Row::new(vec![
            Cell::from(Span::styled("Label", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Host:Port", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("DB User", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Tunnel", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
        ])
        .style(Style::default().bg(bg2()));

        let rows: Vec<Row> = self
            .cfg
            .connections
            .iter()
            .map(|c| {
                Row::new(vec![
                    Cell::from(c.label.clone()),
                    Cell::from(format!("{}:{}", c.host, c.port)),
                    Cell::from(c.db_user.clone()),
                    Cell::from(if c.use_tunnel { "yes" } else { "no" }),
                ])
            })
            .collect();

        let table = Table::new(rows, [Constraint::Length(18), Constraint::Length(26), Constraint::Length(16), Constraint::Length(8)])
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

        let rows2 = conn_form_rows(form_inner, ct.use_tunnel);
        let fw = rows2[0].width.saturating_sub(16) as usize;

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Label:          ", lbl()),
                input_span(&ct.label, ct.field == ConnField::Label, false, fw),
            ])),
            rows2[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Host:           ", lbl()),
                input_span(&ct.host, ct.field == ConnField::Host, false, fw),
            ])),
            rows2[2],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Port:           ", lbl()),
                input_span(&ct.port, ct.field == ConnField::Port, false, fw),
            ])),
            rows2[4],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("DB User:        ", lbl()),
                input_span(&ct.db_user, ct.field == ConnField::DbUser, false, fw),
            ])),
            rows2[6],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "needs CREATEROLE (or superuser) privileges — e.g. the postgres superuser, or a dedicated admin role",
                Style::default().fg(yellow()),
            ))),
            rows2[7],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("DB Password:    ", lbl()),
                input_span(&ct.db_password, ct.field == ConnField::DbPassword, true, fw),
            ])),
            rows2[9],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Use SSH Tunnel: ", lbl()),
                btn_span(if ct.use_tunnel { "Yes" } else { "No" }, ct.field == ConnField::UseTunnel),
                Span::styled("  (\u{2190}/\u{2192} to change)", lbl()),
            ])),
            rows2[11],
        );

        let mut next = 12;
        if ct.use_tunnel {
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("SSH Host:       ", lbl()),
                    input_span(&ct.ssh_host, ct.field == ConnField::SshHost, false, fw),
                ])),
                rows2[next + 1],
            );
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("SSH Port:       ", lbl()),
                    input_span(&ct.ssh_port, ct.field == ConnField::SshPort, false, fw),
                ])),
                rows2[next + 3],
            );
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("SSH User:       ", lbl()),
                    input_span(&ct.ssh_user, ct.field == ConnField::SshUser, false, fw),
                ])),
                rows2[next + 5],
            );
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("SSH Key Path:   ", lbl()),
                    input_span(&ct.ssh_key_path, ct.field == ConnField::SshKeyPath, false, fw),
                ])),
                rows2[next + 7],
            );
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("SSH Password:   ", lbl()),
                    input_span(&ct.ssh_password, ct.field == ConnField::SshPassword, true, fw),
                ])),
                rows2[next + 9],
            );
            next += 10;
        }

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
            rows2[next + 1],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "\u{2191}\u{2193} select row  Enter on SSH Host picks a known host  Tab navigate  Esc back",
                lbl(),
            ))),
            rows2[next + 3],
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
            Cell::from(Span::styled("Role", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Super", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Login", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("CreateDB", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("CreateRole", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
        ])
        .style(Style::default().bg(bg2()));
        let table_rows: Vec<Row> = ut
            .rows
            .iter()
            .map(|u| {
                Row::new(vec![
                    Cell::from(u.name.clone()),
                    Cell::from(if u.superuser { "yes" } else { "" }),
                    Cell::from(if u.can_login { "yes" } else { "" }),
                    Cell::from(if u.create_db { "yes" } else { "" }),
                    Cell::from(if u.create_role { "yes" } else { "" }),
                ])
            })
            .collect();
        let users_title = format!(" Users ({}) ", ut.rows.len());
        let table = Table::new(
            table_rows,
            [
                Constraint::Length(20),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Length(10),
                Constraint::Min(10),
            ],
        )
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
            Paragraph::new(Line::from(Span::styled("Enter on a table row opens Change Password / Drop", lbl()))),
            action_rows[1],
        );

        draw_history(f, &self.history, chunks[3], self.history_scroll);

        if ut.connection_dropdown_open {
            render_dropdown(f, &filtered_connections(&self.cfg, ut.connection_input.value()), ut.connection_idx, top_rows[0], 24, area);
        }
    }

    fn draw_add_user_modal(&self, f: &mut Frame, m: &AddUserModal, area: Rect) {
        let width = 70u16.min(area.width.saturating_sub(4));
        let content_width = width.saturating_sub(4);
        let priv_rows = priv_picker::rows_needed(&m.privileges, content_width);
        // Fixed rows: Username,spacer,Password,spacer,GrantDb,hint,spacer,PrivLabel
        // (8) + priv_rows + spacer,buttons,spacer,navhint (4). Must track
        // priv_rows exactly — an undersized Length collapses rows to zero
        // height instead of erroring.
        let content_rows = 8 + priv_rows + 4;
        let height = (content_rows + 4).min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);
        f.render_widget(Clear, modal_area);
        let block = Block::default()
            .title(Span::styled(" Add PostgreSQL User ", Style::default().fg(title_color())))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(accent()))
            .style(Style::default().bg(bg2()));
        let inner = block.inner(modal_area);
        f.render_widget(block, modal_area);

        let mut constraints = vec![
            Constraint::Length(1), // [0] Username
            Constraint::Length(1), // [1] spacer
            Constraint::Length(1), // [2] Password
            Constraint::Length(1), // [3] spacer
            Constraint::Length(1), // [4] Grant DB
            Constraint::Length(1), // [5] hint
            Constraint::Length(1), // [6] spacer
            Constraint::Length(1), // [7] Privileges label
        ];
        for _ in 0..priv_rows {
            constraints.push(Constraint::Length(1)); // [8..8+priv_rows) picker rows
        }
        constraints.extend([
            Constraint::Length(1), // spacer
            Constraint::Length(1), // buttons
            Constraint::Length(1), // spacer
            Constraint::Length(1), // nav hint
            Constraint::Min(0),
        ]);

        let rows = Layout::default().direction(Direction::Vertical).margin(1).constraints(constraints).split(inner);
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
            Paragraph::new(Line::from(vec![Span::styled("Grant DB: ", lbl()), input_span(&m.grant_db, m.field == AddField::GrantDb, false, fw)])),
            rows[4],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "database to grant the selected privileges on (its public schema). Leave empty to skip granting entirely",
                lbl(),
            ))),
            rows[5],
        );
        f.render_widget(Paragraph::new(Line::from(Span::styled("Privileges (\u{2190}/\u{2192} move, Enter/Space toggle):", lbl()))), rows[7]);
        if priv_rows > 0 {
            let first = rows[8];
            let picker_area = Rect::new(first.x, first.y, first.width, priv_rows);
            priv_picker::draw(f, &m.privileges, m.field == AddField::Privileges, picker_area);
        }
        let after_priv = 8 + priv_rows as usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                btn_span("Create", m.field == AddField::BtnCreate),
                Span::raw("  "),
                btn_span("Cancel", m.field == AddField::BtnCancel),
            ])),
            rows[after_priv + 1],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("Tab navigate  \u{2022}  Enter activate  \u{2022}  Esc cancel", lbl()))),
            rows[after_priv + 3],
        );
    }

    fn draw_edit_user_modal(&self, f: &mut Frame, m: &EditUserModal, area: Rect) {
        let width = 64u16.min(area.width.saturating_sub(4));
        let height = 12u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);
        f.render_widget(Clear, modal_area);
        let block = Block::default()
            .title(Span::styled(format!(" Role \"{}\" ", m.user), Style::default().fg(title_color())))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(accent()))
            .style(Style::default().bg(bg2()));
        let inner = block.inner(modal_area);
        f.render_widget(block, modal_area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
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
        f.render_widget(
            Paragraph::new(Line::from(vec![btn_span("Change Password", m.field == EditField::BtnChangePassword)])),
            rows[2],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                btn_span("Drop User", m.field == EditField::BtnDrop),
                Span::raw("  "),
                btn_span("Cancel", m.field == EditField::BtnCancel),
            ])),
            rows[4],
        );
        f.render_widget(Paragraph::new(Line::from(Span::styled("Tab navigate  \u{2022}  Enter activate  \u{2022}  Esc cancel", lbl()))), rows[6]);
    }
}

/// Matches by label *or* host, so typing a connection's IP finds it just
/// as well as typing its name — the field only ever shows/stores the
/// label, this just widens what a keystroke can match against.
fn filtered_connections(cfg: &config::Config, query: &str) -> Vec<String> {
    let q = query.to_lowercase();
    cfg.connections
        .iter()
        .filter(|c| q.is_empty() || c.label.to_lowercase().contains(&q) || c.host.to_lowercase().contains(&q))
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

