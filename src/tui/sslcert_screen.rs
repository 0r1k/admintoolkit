//! SSL Certificate Manager screen — connects to a local-or-remote host
//! (Target tab, same shape every other atk screen uses), scans whatever
//! is answering on :443, and lists every HTTPS vhost it found with its
//! domains, cert file, CA/chain file (if separate) and expiry
//! (Certificates tab). Selecting one and pressing Enter walks through a
//! local file picker for the new cert, a keyboard-only Yes/No on whether
//! a separate CA/chain file is also needed, and — if so — a second file
//! picker for it, before writing anything remote.

use std::{sync::mpsc, thread};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};

use crate::sslcert::{
    detect::{self, DetectResult, VHost, WebServer},
    engine::{self, UpdateInput},
    exec::{ExecSession, Target},
};

use super::file_picker::FilePicker;
use super::host_picker::HostPicker;
use super::mouse;
use super::widgets::*;
use super::{file_picker, host_picker};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Target,
    Certs,
}

// ── Target tab — same shape as every other atk screen's connect form,
// minus the Local/Remote toggle: certs live on the servers being
// administered, never on the machine atk itself runs on, so this screen
// is remote-only. ─────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetField {
    SshHost,
    SshPort,
    SshUser,
    SshKeyPath,
    SshPassword,
    BtnConnect,
}

fn target_active_fields() -> Vec<TargetField> {
    vec![
        TargetField::SshHost,
        TargetField::SshPort,
        TargetField::SshUser,
        TargetField::SshKeyPath,
        TargetField::SshPassword,
        TargetField::BtnConnect,
    ]
}

fn target_form_rows(form_inner: Rect) -> Vec<Rect> {
    let constraints = vec![
        Constraint::Length(1), // SshHost
        Constraint::Length(1), // spacer
        Constraint::Length(1), // SshPort
        Constraint::Length(1), // spacer
        Constraint::Length(1), // SshUser
        Constraint::Length(1), // spacer
        Constraint::Length(1), // SshKeyPath
        Constraint::Length(1), // spacer
        Constraint::Length(1), // SshPassword
        Constraint::Length(1), // spacer
        Constraint::Length(1), // Connect button
        Constraint::Length(1), // spacer
        Constraint::Length(1), // nav hint
        Constraint::Min(0),
    ];
    Layout::default().direction(Direction::Vertical).margin(1).constraints(constraints).split(form_inner).to_vec()
}

struct TargetTab {
    ssh_host: Input,
    ssh_port: Input,
    ssh_user: Input,
    ssh_key_path: Input,
    ssh_password: Input,
    field: TargetField,
    host_picker: Option<HostPicker>,
    key_picker: Option<FilePicker>,
    connecting: bool,
}

impl TargetTab {
    fn new() -> Self {
        Self {
            ssh_host: Input::default(),
            ssh_port: Input::new("22"),
            ssh_user: Input::default(),
            ssh_key_path: Input::default(),
            ssh_password: Input::default(),
            field: TargetField::SshHost,
            host_picker: None,
            key_picker: None,
            connecting: false,
        }
    }

    fn next_field(&mut self) {
        let fields = target_active_fields();
        let idx = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = fields[(idx + 1) % fields.len()];
    }

    fn prev_field(&mut self) {
        let fields = target_active_fields();
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
        Target::Remote {
            host: self.ssh_host.value().trim().to_string(),
            port: self.ssh_port.value().trim().to_string(),
            user: self.ssh_user.value().trim().to_string(),
            key_path: self.ssh_key_path.value().trim().to_string(),
            password: self.ssh_password.value().to_string(),
        }
    }
}

// ── Update flow — file pickers + CA confirm, walked one step at a time ────
enum UpdateFlow {
    PickCert(FilePicker),
    AskCa { cert_path: String, cert_content: String, bundled_hint: bool },
    PickCa { cert_path: String, cert_content: String, picker: FilePicker },
}

enum Msg {
    Connected { label: String, result: Result<DetectResult, String> },
    Rescanned(Result<DetectResult, String>),
    UpdateResult { idx: usize, result: Result<engine::UpdateResult, String> },
}

pub struct SslCertScreen {
    tab: Tab,
    target_tab: TargetTab,
    target_label: String,
    connected: bool,
    server: Option<WebServer>,
    vhosts: Vec<VHost>,
    selected_row: usize,
    update_vhost_idx: usize,
    update_flow: Option<UpdateFlow>,
    modal: Option<(String, String)>,
    history: Vec<(bool, String)>,
    history_scroll: u16,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
}

impl SslCertScreen {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tab: Tab::Target,
            target_tab: TargetTab::new(),
            target_label: String::new(),
            connected: false,
            server: None,
            vhosts: Vec::new(),
            selected_row: 0,
            update_vhost_idx: 0,
            update_flow: None,
            modal: None,
            history: Vec::new(),
            history_scroll: 0,
            tx,
            rx,
        }
    }

    pub fn tick(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Connected { label, result } => {
                    self.target_tab.connecting = false;
                    match result {
                        Ok(dr) => {
                            self.connected = true;
                            self.target_label = label.clone();
                            let summary = scan_summary(&dr.server, dr.vhosts.len());
                            self.server = Some(dr.server);
                            self.vhosts = dr.vhosts;
                            self.vhosts.sort_by_key(expiry_sort_key);
                            self.selected_row = 0;
                            self.history.push((true, format!("Connected to {label}")));
                            self.history.push((true, summary));
                            self.tab = Tab::Certs;
                        }
                        Err(e) => {
                            self.connected = false;
                            self.history.push((false, format!("Connect/scan failed: {}", one_line(&e))));
                            self.modal = Some(("Connect / scan failed".into(), e));
                        }
                    }
                }
                Msg::Rescanned(result) => match result {
                    Ok(dr) => {
                        let prev_cert = self.vhosts.get(self.selected_row).map(|v| v.cert_file.clone());
                        let summary = scan_summary(&dr.server, dr.vhosts.len());
                        self.server = Some(dr.server);
                        self.vhosts = dr.vhosts;
                        self.vhosts.sort_by_key(expiry_sort_key);
                        self.reselect(prev_cert);
                        self.history.push((true, summary));
                    }
                    Err(e) => {
                        self.history.push((false, format!("Rescan failed: {}", one_line(&e))));
                        self.modal = Some(("Rescan failed".into(), e));
                    }
                },
                Msg::UpdateResult { idx, result } => match result {
                    Ok(ur) => {
                        let updated_cert = self.vhosts.get(idx).map(|v| v.cert_file.clone());
                        if let Some(v) = self.vhosts.get_mut(idx) {
                            v.not_after = ur.new_not_after;
                            v.days_left = ur.new_days_left;
                            v.cert_error = None;
                        }
                        self.vhosts.sort_by_key(expiry_sort_key);
                        self.reselect(updated_cert);
                        for m in ur.messages {
                            self.history.push((true, m));
                        }
                        self.history.push((true, "Certificate update complete".to_string()));
                    }
                    Err(e) => {
                        self.history.push((false, format!("Update failed: {}", one_line(&e))));
                        self.modal = Some(("Update failed".into(), e));
                    }
                },
            }
        }
    }

    fn error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.history.push((false, msg.clone()));
        self.modal = Some(("Error".into(), msg));
    }

    /// Points `selected_row` back at the vhost identified by `cert_file`
    /// (its path is unique per row) after `self.vhosts` was just
    /// re-sorted — otherwise the cursor would silently jump to whatever
    /// vhost happens to now sit at the old numeric index.
    fn reselect(&mut self, cert_file: Option<String>) {
        self.selected_row = cert_file
            .and_then(|cert| self.vhosts.iter().position(|v| v.cert_file == cert))
            .unwrap_or_else(|| self.selected_row.min(self.vhosts.len().saturating_sub(1)));
    }

    // ── Key handling ─────────────────────────────────────────────────
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.modal.is_some() {
            match key.code {
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => self.modal = None,
                _ => {}
            }
            return false;
        }
        if self.update_flow.is_some() {
            self.handle_update_flow_key(key);
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
            self.history_scroll = if key.code == KeyCode::Up { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            return false;
        }

        let pickers_open = self.target_tab.host_picker.is_some() || self.target_tab.key_picker.is_some();

        match key.code {
            KeyCode::Esc if !pickers_open => return true,
            KeyCode::F(1) if !pickers_open => {
                self.tab = Tab::Target;
                return false;
            }
            KeyCode::F(2) if !pickers_open => {
                self.tab = Tab::Certs;
                return false;
            }
            _ => {}
        }

        match self.tab {
            Tab::Target => self.handle_target_key(key),
            Tab::Certs => self.handle_certs_key(key),
        }
        false
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

    fn trigger_connect(&mut self) {
        if self.target_tab.ssh_host.value().trim().is_empty() {
            self.error("Enter a remote host first");
            return;
        }
        let target = self.target_tab.as_target();
        self.target_tab.connecting = true;
        let label = target.label();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = (|| -> Result<DetectResult, String> {
                let session = ExecSession::open(&target)?;
                detect::detect(&session)
            })();
            let _ = tx.send(Msg::Connected { label, result });
        });
    }

    fn trigger_rescan(&mut self) {
        if !self.connected {
            return;
        }
        let target = self.target_tab.as_target();
        let tx = self.tx.clone();
        self.history.push((true, "Rescanning…".to_string()));
        thread::spawn(move || {
            let result = (|| -> Result<DetectResult, String> {
                let session = ExecSession::open(&target)?;
                detect::detect(&session)
            })();
            let _ = tx.send(Msg::Rescanned(result));
        });
    }

    fn handle_certs_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if self.selected_row > 0 {
                    self.selected_row -= 1;
                }
            }
            KeyCode::Down => {
                if self.selected_row + 1 < self.vhosts.len() {
                    self.selected_row += 1;
                }
            }
            KeyCode::Enter => self.start_update_flow(),
            KeyCode::Char('r') | KeyCode::Char('R') => self.trigger_rescan(),
            _ => {}
        }
    }

    fn start_update_flow(&mut self) {
        if !self.connected {
            self.error("Connect on the Target tab first");
            return;
        }
        if self.vhosts.is_empty() {
            return;
        }
        self.update_vhost_idx = self.selected_row;
        self.update_flow = Some(UpdateFlow::PickCert(FilePicker::new("~")));
    }

    fn handle_update_flow_key(&mut self, key: KeyEvent) {
        let Some(flow) = self.update_flow.take() else { return };
        match flow {
            UpdateFlow::PickCert(mut picker) => match key.code {
                KeyCode::Esc => {}
                KeyCode::Up => {
                    picker.up();
                    self.update_flow = Some(UpdateFlow::PickCert(picker));
                }
                KeyCode::Down => {
                    picker.down();
                    self.update_flow = Some(UpdateFlow::PickCert(picker));
                }
                KeyCode::Enter => match picker.activate() {
                    None => self.update_flow = Some(UpdateFlow::PickCert(picker)),
                    Some(path) => self.cert_picked(path),
                },
                _ => self.update_flow = Some(UpdateFlow::PickCert(picker)),
            },
            UpdateFlow::AskCa { cert_path, cert_content, bundled_hint } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter => {
                    let idx = self.update_vhost_idx;
                    self.spawn_update(idx, cert_content, None);
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.update_flow = Some(UpdateFlow::PickCa { cert_path, cert_content, picker: FilePicker::new("~") });
                }
                _ => self.update_flow = Some(UpdateFlow::AskCa { cert_path, cert_content, bundled_hint }),
            },
            UpdateFlow::PickCa { cert_path, cert_content, mut picker } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Up => {
                    picker.up();
                    self.update_flow = Some(UpdateFlow::PickCa { cert_path, cert_content, picker });
                }
                KeyCode::Down => {
                    picker.down();
                    self.update_flow = Some(UpdateFlow::PickCa { cert_path, cert_content, picker });
                }
                KeyCode::Enter => match picker.activate() {
                    None => self.update_flow = Some(UpdateFlow::PickCa { cert_path, cert_content, picker }),
                    Some(path) => self.ca_picked(cert_content, path),
                },
                _ => self.update_flow = Some(UpdateFlow::PickCa { cert_path, cert_content, picker }),
            },
        }
    }

    fn cert_picked(&mut self, path: std::path::PathBuf) {
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let bundled_hint = content.matches("-----BEGIN CERTIFICATE-----").count() > 1;
                self.update_flow = Some(UpdateFlow::AskCa { cert_path: path.to_string_lossy().to_string(), cert_content: content, bundled_hint });
            }
            Err(e) => self.error(format!("couldn't read {}: {e}", path.display())),
        }
    }

    fn ca_picked(&mut self, cert_content: String, path: std::path::PathBuf) {
        match std::fs::read_to_string(&path) {
            Ok(ca_content) => {
                let idx = self.update_vhost_idx;
                self.spawn_update(idx, cert_content, Some(ca_content));
            }
            Err(e) => self.error(format!("couldn't read {}: {e}", path.display())),
        }
    }

    fn spawn_update(&mut self, idx: usize, cert_content: String, ca_content: Option<String>) {
        let (Some(vhost), Some(server)) = (self.vhosts.get(idx).cloned(), self.server.clone()) else {
            self.error("nothing selected to update");
            return;
        };
        self.history.push((true, format!("Updating certificate for {} …", vhost.domains.join(", "))));
        let target = self.target_tab.as_target();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = (|| -> Result<engine::UpdateResult, String> {
                let session = ExecSession::open(&target)?;
                engine::update(&session, UpdateInput { vhost, server_kind: server.kind, cert_content, ca_content })
            })();
            let _ = tx.send(Msg::UpdateResult { idx, result });
        });
    }

    // ── Mouse handling ───────────────────────────────────────────────
    pub fn handle_mouse(&mut self, me: MouseEvent, area: Rect) {
        if self.modal.is_some() {
            return;
        }
        // Keyboard-only for the CA choice — it decides whether a config
        // file gets edited, same "no stray click" rule the risky-change
        // confirm elsewhere in atk follows.
        if matches!(self.update_flow, Some(UpdateFlow::AskCa { .. })) {
            return;
        }
        if self.update_flow.is_some() {
            self.handle_update_flow_mouse(me, area);
            return;
        }

        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(area);

        if let Some((x, y)) = mouse::left_click(&me) {
            if let Some(i) = mouse::label_row_hit(x, y, chunks[0], &["F1 Target", "F2 Certificates"]) {
                self.tab = if i == 0 { Tab::Target } else { Tab::Certs };
                return;
            }
        }

        match self.tab {
            Tab::Target => self.handle_target_mouse(me, chunks[1]),
            Tab::Certs => self.handle_certs_mouse(me, chunks[1]),
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

        let chunks =
            Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(18), Constraint::Min(3), Constraint::Length(8)]).split(area);

        if let Some(delta) = mouse::scroll_delta(&me) {
            if mouse::in_rect(chunks[2], me.column, me.row) {
                self.history_scroll = if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };

        let form_inner = mouse::block_inner(chunks[0]);
        let rows = target_form_rows(form_inner);

        if mouse::button_row_hit(x, y, rows[10], &["Connect"]).is_some() {
            self.trigger_connect();
            return;
        }

        let field_rows: &[(usize, TargetField)] = &[
            (0, TargetField::SshHost),
            (2, TargetField::SshPort),
            (4, TargetField::SshUser),
            (6, TargetField::SshKeyPath),
            (8, TargetField::SshPassword),
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

    fn handle_certs_mouse(&mut self, me: MouseEvent, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3), Constraint::Min(6), Constraint::Length(8)]).split(area);

        if let Some(delta) = mouse::scroll_delta(&me) {
            let n = self.vhosts.len();
            if n > 0 && mouse::in_rect(chunks[1], me.column, me.row) {
                if delta < 0 && self.selected_row > 0 {
                    self.selected_row -= 1;
                } else if delta > 0 && self.selected_row + 1 < n {
                    self.selected_row += 1;
                }
            } else if mouse::in_rect(chunks[2], me.column, me.row) {
                self.history_scroll = if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };
        if let Some(idx) = mouse::table_row_hit(x, y, chunks[1], 1, self.vhosts.len(), self.selected_row) {
            self.selected_row = idx;
            self.start_update_flow();
        }
    }

    fn handle_update_flow_mouse(&mut self, me: MouseEvent, area: Rect) {
        match self.update_flow.take() {
            Some(UpdateFlow::PickCert(mut picker)) => {
                if let Some((x, y)) = mouse::left_click(&me) {
                    match picker.row_at(area, x, y) {
                        Some(idx) => {
                            picker.selected = idx;
                            match picker.activate() {
                                None => self.update_flow = Some(UpdateFlow::PickCert(picker)),
                                Some(path) => self.cert_picked(path),
                            }
                        }
                        None => self.update_flow = Some(UpdateFlow::PickCert(picker)),
                    }
                } else if let Some(delta) = mouse::scroll_delta(&me) {
                    if delta < 0 {
                        picker.up();
                    } else {
                        picker.down();
                    }
                    self.update_flow = Some(UpdateFlow::PickCert(picker));
                } else {
                    self.update_flow = Some(UpdateFlow::PickCert(picker));
                }
            }
            Some(UpdateFlow::PickCa { cert_path, cert_content, mut picker }) => {
                if let Some((x, y)) = mouse::left_click(&me) {
                    match picker.row_at(area, x, y) {
                        Some(idx) => {
                            picker.selected = idx;
                            match picker.activate() {
                                None => self.update_flow = Some(UpdateFlow::PickCa { cert_path, cert_content, picker }),
                                Some(path) => self.ca_picked(cert_content, path),
                            }
                        }
                        None => self.update_flow = Some(UpdateFlow::PickCa { cert_path, cert_content, picker }),
                    }
                } else if let Some(delta) = mouse::scroll_delta(&me) {
                    if delta < 0 {
                        picker.up();
                    } else {
                        picker.down();
                    }
                    self.update_flow = Some(UpdateFlow::PickCa { cert_path, cert_content, picker });
                } else {
                    self.update_flow = Some(UpdateFlow::PickCa { cert_path, cert_content, picker });
                }
            }
            other => self.update_flow = other,
        }
    }

    // ── Draw ─────────────────────────────────────────────────────────
    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(area);

        let tab_bar = Line::from(vec![
            tab_span("F1 Target", self.tab == Tab::Target),
            Span::styled("  ", Style::default().bg(bg())),
            tab_span("F2 Certificates", self.tab == Tab::Certs),
            Span::styled("  ", Style::default().bg(bg())),
            Span::styled("Esc back  Ctrl+C quit", Style::default().fg(fg2()).bg(bg())),
        ]);
        f.render_widget(Paragraph::new(tab_bar), chunks[0]);

        match self.tab {
            Tab::Target => self.draw_target(f, chunks[1]),
            Tab::Certs => self.draw_certs(f, chunks[1]),
        }

        if let Some(flow) = &self.update_flow {
            self.draw_update_flow(f, flow, area);
        }
        if let Some((title, msg)) = &self.modal {
            draw_modal(f, title, msg, area);
        }
    }

    fn draw_target(&self, f: &mut Frame, area: Rect) {
        let chunks =
            Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(18), Constraint::Min(3), Constraint::Length(8)]).split(area);

        let form_block = theme_block(" Target ");
        let form_inner = form_block.inner(chunks[0]);
        f.render_widget(form_block, chunks[0]);
        let rows = target_form_rows(form_inner);
        let tt = &self.target_tab;

        let w = |r: Rect| (r.width as usize).saturating_sub(12).max(6);
        f.render_widget(
            Line::from(vec![Span::styled("SSH Host   ", lbl()), input_span(&tt.ssh_host, tt.field == TargetField::SshHost, false, w(rows[0]))]),
            rows[0],
        );
        f.render_widget(
            Line::from(vec![Span::styled("SSH Port   ", lbl()), input_span(&tt.ssh_port, tt.field == TargetField::SshPort, false, w(rows[2]))]),
            rows[2],
        );
        f.render_widget(
            Line::from(vec![Span::styled("SSH User   ", lbl()), input_span(&tt.ssh_user, tt.field == TargetField::SshUser, false, w(rows[4]))]),
            rows[4],
        );
        f.render_widget(
            Line::from(vec![Span::styled("Key Path   ", lbl()), input_span(&tt.ssh_key_path, tt.field == TargetField::SshKeyPath, false, w(rows[6]))]),
            rows[6],
        );
        f.render_widget(
            Line::from(vec![
                Span::styled("Password   ", lbl()),
                input_span(&tt.ssh_password, tt.field == TargetField::SshPassword, true, w(rows[8])),
            ]),
            rows[8],
        );

        f.render_widget(Line::from(vec![btn_span("Connect", tt.field == TargetField::BtnConnect)]), rows[10]);

        f.render_widget(Paragraph::new(Line::from(Span::styled("Tab/Shift+Tab move  Enter activate  Esc back", lbl()))), rows[12]);

        if let Some(picker) = &tt.host_picker {
            host_picker::draw(f, picker, chunks[1]);
        } else if let Some(picker) = &tt.key_picker {
            file_picker::draw(f, picker, chunks[1]);
        } else {
            let info_block = theme_block(" Target Info ");
            let info_inner = info_block.inner(chunks[1]);
            f.render_widget(info_block, chunks[1]);
            let lines: Vec<Line> = if tt.connecting {
                vec![Line::from(Span::styled("Connecting and scanning port 443…", Style::default().fg(yellow())))]
            } else if self.connected {
                let mut ls = vec![Line::from(vec![Span::styled("Connected to: ", lbl()), Span::styled(self.target_label.clone(), Style::default().fg(green()))])];
                if let Some(s) = &self.server {
                    ls.push(Line::from(vec![Span::styled("Web server: ", lbl()), Span::raw(scan_summary(s, self.vhosts.len()))]));
                }
                ls.push(Line::from(Span::styled("See the Certificates tab (F2) for details.", lbl())));
                ls
            } else {
                vec![Line::from(Span::styled(
                    "Not connected yet. Fill in the remote host above, then press Connect. Nothing on :443 is treated as an error, not silently ignored.",
                    lbl(),
                ))]
            };
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), info_inner);
        }

        draw_history(f, &self.history, chunks[2], self.history_scroll);
    }

    fn draw_certs(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3), Constraint::Min(6), Constraint::Length(8)]).split(area);

        let info_block = theme_block(" Web Server ");
        let info_inner = info_block.inner(chunks[0]);
        f.render_widget(info_block, chunks[0]);
        let info_line = match &self.server {
            Some(s) if s.kind != "unknown" => Line::from(vec![
                Span::styled(s.kind.clone(), Style::default().fg(accent()).add_modifier(Modifier::BOLD)),
                Span::raw(format!(" {}   ", if s.version.is_empty() { "(version unknown)".to_string() } else { s.version.clone() })),
                Span::styled("pid ", lbl()),
                Span::raw(s.pid.clone()),
                Span::raw("   "),
                Span::styled("binary ", lbl()),
                Span::raw(s.binary.clone()),
            ]),
            Some(_) => Line::from(Span::styled(
                "Something is listening on :443, but it wasn't recognized as nginx or apache — no config parsing available for it.",
                Style::default().fg(yellow()),
            )),
            None => Line::from(Span::styled("Not scanned yet — connect on the Target tab (F1) first.", lbl())),
        };
        f.render_widget(Paragraph::new(info_line).wrap(Wrap { trim: false }), info_inner);

        let header = Row::new(vec![
            Cell::from(Span::styled("Domains", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Certificate file", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("CA / chain", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Expires", Style::default().fg(title_color()).add_modifier(Modifier::BOLD))),
        ])
        .style(Style::default().bg(bg2()));

        let rows: Vec<Row> = self
            .vhosts
            .iter()
            .map(|v| {
                let ca = match &v.ca_file {
                    Some(p) => p.clone(),
                    None => "(bundled / none)".to_string(),
                };
                let expires: Cell = match (&v.cert_error, v.days_left) {
                    (Some(e), _) => Cell::from(Span::styled(format!("error: {}", one_line(e)), Style::default().fg(red()))),
                    (None, Some(d)) => Cell::from(Span::styled(format!("{} ({})", v.not_after, fmt_days(d)), Style::default().fg(expiry_color(d)))),
                    (None, None) => Cell::from(Span::styled("n/a", lbl())),
                };
                Row::new(vec![Cell::from(v.domains.join(", ")), Cell::from(v.cert_file.clone()), Cell::from(ca), expires])
            })
            .collect();

        let table = Table::new(rows, [Constraint::Percentage(28), Constraint::Percentage(28), Constraint::Percentage(22), Constraint::Percentage(22)])
            .header(header)
            .block(theme_block(" HTTPS Vhosts — Enter to update the selected certificate, R rescan "))
            .row_highlight_style(focused())
            .highlight_symbol(" \u{25B6} ")
            .style(Style::default().fg(fg()).bg(bg()));
        let mut tstate = TableState::default();
        if !self.vhosts.is_empty() {
            tstate.select(Some(self.selected_row.min(self.vhosts.len() - 1)));
        }
        f.render_stateful_widget(table, chunks[1], &mut tstate);

        draw_history(f, &self.history, chunks[2], self.history_scroll);
    }

    fn draw_update_flow(&self, f: &mut Frame, flow: &UpdateFlow, area: Rect) {
        match flow {
            UpdateFlow::PickCert(picker) => file_picker::draw(f, picker, area),
            UpdateFlow::PickCa { picker, .. } => file_picker::draw(f, picker, area),
            UpdateFlow::AskCa { cert_path, bundled_hint, .. } => {
                let vhost = self.vhosts.get(self.update_vhost_idx);
                let existing_ca = vhost.and_then(|v| v.ca_file.clone());
                let key_file = vhost.map(|v| v.key_file.clone()).unwrap_or_default();
                let mut msg = format!("New certificate file:\n  {cert_path}\n\nPrivate key file (left untouched): {key_file}\n\n");
                if *bundled_hint {
                    msg.push_str("This file contains more than one certificate block — it looks like it already bundles a CA chain.\n\n");
                }
                match existing_ca {
                    Some(existing) => msg.push_str(&format!("This vhost currently points at a separate CA/chain file:\n  {existing}\n\nDoes the new certificate also need one?\n")),
                    None => msg.push_str("This vhost has no separate CA/chain directive right now.\n\nDoes the new certificate need one?\n"),
                }
                msg.push_str("\n[N] / Enter — no, this file is everything\n[Y] — yes, pick a CA/chain file next\n[Esc] cancel  (keyboard only)");

                let width = 74u16.min(area.width.saturating_sub(4));
                let height = (msg.lines().count() as u16 + 3).min(area.height.saturating_sub(2)).max(10);
                let modal_area = centered_rect(width, height, area);
                f.render_widget(Clear, modal_area);
                f.render_widget(
                    Paragraph::new(msg)
                        .block(
                            Block::default()
                                .title(Span::styled(" Separate CA / chain file? ", Style::default().fg(yellow())))
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(yellow())),
                        )
                        .style(Style::default().fg(fg()).bg(bg2()))
                        .wrap(Wrap { trim: true }),
                    modal_area,
                );
            }
        }
    }
}

/// Ascending sort key for the vhost table: certs that errored while
/// reading their expiry sort first (they need attention and their
/// urgency can't be ranked, so treat them as the most urgent), then
/// whatever's genuinely closest to expiring, then anything with no
/// expiry data at all last.
fn expiry_sort_key(v: &VHost) -> i64 {
    match (&v.cert_error, v.days_left) {
        (Some(_), _) => i64::MIN,
        (None, Some(d)) => d,
        (None, None) => i64::MAX,
    }
}

fn scan_summary(server: &WebServer, vhost_count: usize) -> String {
    let version = if server.version.is_empty() { "unknown version".to_string() } else { server.version.clone() };
    if server.kind == "unknown" {
        format!("unrecognized process on :443 (pid {}) — no config parsing available", server.pid)
    } else {
        format!("{} {} (pid {}) — {} HTTPS vhost(s) on :443", server.kind, version, server.pid, vhost_count)
    }
}

fn fmt_days(d: i64) -> String {
    if d < 0 {
        format!("expired {}d ago", -d)
    } else {
        format!("{d}d left")
    }
}

fn expiry_color(days: i64) -> Color {
    if days <= 0 {
        red()
    } else if days <= 30 {
        yellow()
    } else {
        green()
    }
}
