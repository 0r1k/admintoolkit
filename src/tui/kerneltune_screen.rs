//! Kernel Tuner screen — browse a curated sysctl/sysfs/ulimit catalog,
//! stage changes (by hand or by picking a usage-scenario profile), review
//! them with an explicit opt-in "persist across reboots" toggle (runtime
//! -only is the default for everything that has a runtime-only form), and
//! revert anything atk itself has applied. Works against localhost or a
//! remote host over SSH, picked on the Target tab.

use std::{collections::HashMap, sync::mpsc, thread};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};

use crate::kerneltune::{
    catalog::{self, Category, Kind, Profile, Risk, Tunable},
    engine::{self, TargetInfo},
    exec::{ExecSession, Target},
    store,
};

use super::file_picker::FilePicker;
use super::host_picker::HostPicker;
use super::mouse;
use super::widgets::*;
use super::{file_picker, host_picker};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Target,
    Catalog,
    Review,
    Revert,
}

// ── Target tab ───────────────────────────────────────────────────────────
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

/// Shared between `draw_target` and `handle_target_mouse` so the two can't
/// drift apart — same pattern every other screen's form uses.
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

// ── Catalog tab ──────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum CatalogField {
    ProfileFilter,
    CategoryFilter,
    Table,
    BtnApplyProfile,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailField {
    Value,
    BtnUseRecommended,
    BtnStage,
    BtnCancel,
}

struct DetailModal {
    key: &'static str,
    value: Input,
    field: DetailField,
}

impl DetailModal {
    fn open(t: &'static Tunable, current: Option<&str>, staged: Option<&str>, profile_filter: Option<Profile>) -> Self {
        let mut initial = staged.or(current).unwrap_or("").to_string();
        if initial.is_empty() {
            if let Some(rec) = profile_filter.and_then(|p| t.recommended_for(p)) {
                initial = rec.value.to_string();
            }
        }
        Self { key: t.key, value: Input::new(&initial), field: DetailField::Value }
    }

    fn next_field(&mut self) {
        self.field = match self.field {
            DetailField::Value => DetailField::BtnUseRecommended,
            DetailField::BtnUseRecommended => DetailField::BtnStage,
            DetailField::BtnStage => DetailField::BtnCancel,
            DetailField::BtnCancel => DetailField::Value,
        };
    }

    fn prev_field(&mut self) {
        self.field = match self.field {
            DetailField::Value => DetailField::BtnCancel,
            DetailField::BtnUseRecommended => DetailField::Value,
            DetailField::BtnStage => DetailField::BtnUseRecommended,
            DetailField::BtnCancel => DetailField::BtnStage,
        };
    }
}

struct CatalogTab {
    profile_filter: Option<Profile>,
    category_filter: Option<Category>,
    selected_row: usize,
    field: CatalogField,
    detail: Option<DetailModal>,
}

impl CatalogTab {
    fn new() -> Self {
        Self { profile_filter: None, category_filter: None, selected_row: 0, field: CatalogField::Table, detail: None }
    }

    fn filtered(&self) -> Vec<&'static Tunable> {
        catalog::CATALOG
            .iter()
            .filter(|t| self.category_filter.map(|c| t.category == c).unwrap_or(true))
            .filter(|t| self.profile_filter.map(|p| t.recommended_for(p).is_some()).unwrap_or(true))
            .collect()
    }

    fn next_field(&mut self) {
        self.field = match self.field {
            CatalogField::ProfileFilter => CatalogField::CategoryFilter,
            CatalogField::CategoryFilter => CatalogField::Table,
            CatalogField::Table => CatalogField::BtnApplyProfile,
            CatalogField::BtnApplyProfile => CatalogField::ProfileFilter,
        };
    }

    fn prev_field(&mut self) {
        self.field = match self.field {
            CatalogField::ProfileFilter => CatalogField::BtnApplyProfile,
            CatalogField::CategoryFilter => CatalogField::ProfileFilter,
            CatalogField::Table => CatalogField::CategoryFilter,
            CatalogField::BtnApplyProfile => CatalogField::Table,
        };
    }

    fn cycle_profile(&mut self, delta: i32) {
        let all = Profile::ALL;
        let cur = match self.profile_filter {
            None => -1,
            Some(p) => all.iter().position(|x| *x == p).unwrap_or(0) as i32,
        };
        let len = all.len() as i32;
        let new = (cur + delta + 1).rem_euclid(len + 1) - 1;
        self.profile_filter = if new < 0 { None } else { Some(all[new as usize]) };
        self.selected_row = 0;
    }

    fn cycle_category(&mut self, delta: i32) {
        let all = Category::ALL;
        let cur = match self.category_filter {
            None => -1,
            Some(c) => all.iter().position(|x| *x == c).unwrap_or(0) as i32,
        };
        let len = all.len() as i32;
        let new = (cur + delta + 1).rem_euclid(len + 1) - 1;
        self.category_filter = if new < 0 { None } else { Some(all[new as usize]) };
        self.selected_row = 0;
    }
}

// ── Review tab ───────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReviewField {
    Table,
    BtnPersistAll,
    BtnApplyAll,
    BtnClearAll,
}

struct ReviewTab {
    field: ReviewField,
    selected_row: usize,
}

impl ReviewTab {
    fn new() -> Self {
        Self { field: ReviewField::Table, selected_row: 0 }
    }

    fn next_field(&mut self) {
        self.field = match self.field {
            ReviewField::Table => ReviewField::BtnPersistAll,
            ReviewField::BtnPersistAll => ReviewField::BtnApplyAll,
            ReviewField::BtnApplyAll => ReviewField::BtnClearAll,
            ReviewField::BtnClearAll => ReviewField::Table,
        };
    }

    fn prev_field(&mut self) {
        self.field = match self.field {
            ReviewField::Table => ReviewField::BtnClearAll,
            ReviewField::BtnPersistAll => ReviewField::Table,
            ReviewField::BtnApplyAll => ReviewField::BtnPersistAll,
            ReviewField::BtnClearAll => ReviewField::BtnApplyAll,
        };
    }
}

#[derive(Clone)]
struct StagedChange {
    key: &'static str,
    previous: String,
    value: String,
    persist: bool,
}

struct ConfirmModal {
    lines: Vec<String>,
}

// ── Revert tab ───────────────────────────────────────────────────────────
struct RevertTab {
    selected_row: usize,
}

// ── Screen ───────────────────────────────────────────────────────────────
enum Msg {
    Log(bool, String),
    Connected { label: String, info: TargetInfo, values: HashMap<String, String> },
    ConnectFailed(String),
    ApplyResult { key: &'static str, ok: bool, message: String, previous: String, new_value: String, persist: bool },
    ApplyDone,
    RevertResult { key: String, ok: bool, message: String, target: String, previous: String },
    RevertDone,
}

pub struct KernelTuneScreen {
    tab: Tab,
    target_tab: TargetTab,
    catalog_tab: CatalogTab,
    review_tab: ReviewTab,
    revert_tab: RevertTab,
    confirm_apply: Option<ConfirmModal>,
    modal: Option<(String, String)>,

    target_label: String,
    target_info: Option<TargetInfo>,
    current_values: HashMap<String, String>,
    staged: Vec<StagedChange>,
    history_entries: Vec<store::HistoryEntry>,

    history: Vec<(bool, String)>,
    history_scroll: u16,

    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
}

impl KernelTuneScreen {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tab: Tab::Target,
            target_tab: TargetTab::new(),
            catalog_tab: CatalogTab::new(),
            review_tab: ReviewTab::new(),
            revert_tab: RevertTab { selected_row: 0 },
            confirm_apply: None,
            modal: None,
            target_label: String::new(),
            target_info: None,
            current_values: HashMap::new(),
            staged: Vec::new(),
            history_entries: store::load(),
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
                Msg::Connected { label, info, values } => {
                    self.target_tab.connecting = false;
                    self.target_label = label.clone();
                    self.target_info = Some(info);
                    self.current_values = values;
                    self.staged.clear();
                    self.history.push((true, format!("Connected to {label}")));
                }
                Msg::ConnectFailed(e) => {
                    self.target_tab.connecting = false;
                    self.history.push((false, format!("Connect failed: {}", one_line(&e))));
                }
                Msg::ApplyResult { key, ok, message, previous, new_value, persist } => {
                    if ok {
                        self.current_values.insert(key.to_string(), new_value.clone());
                        self.staged.retain(|s| s.key != key);
                        if let Some(t) = catalog::by_key(key) {
                            self.history_entries =
                                store::record(self.history_entries.clone(), &self.target_label, t, &previous, &new_value, persist);
                        }
                        let suffix = if persist { " [persisted]" } else { " [this session only]" };
                        self.history.push((true, format!("{key}: {previous} -> {new_value}{suffix} ({message})")));
                    } else {
                        self.history.push((false, format!("{key}: {message}")));
                    }
                }
                Msg::ApplyDone => self.history.push((true, "Apply finished".to_string())),
                Msg::RevertResult { key, ok, message, target, previous } => {
                    if ok {
                        self.current_values.insert(key.clone(), previous);
                        self.history_entries = store::remove(self.history_entries.clone(), &target, &key);
                        self.history.push((true, format!("Reverted {key}")));
                    } else {
                        self.history.push((false, format!("Revert {key} failed: {message}")));
                    }
                }
                Msg::RevertDone => self.history.push((true, "Revert finished".to_string())),
            }
        }
    }

    fn error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.history.push((false, msg.clone()));
        self.modal = Some(("Error".into(), msg));
    }

    fn filtered_history(&self) -> Vec<store::HistoryEntry> {
        self.history_entries.iter().filter(|e| e.target == self.target_label).cloned().collect()
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
        if self.confirm_apply.is_some() {
            match key.code {
                KeyCode::Enter => {
                    self.confirm_apply = None;
                    self.spawn_apply_all();
                }
                KeyCode::Esc => self.confirm_apply = None,
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

        let pickers_open = self.target_tab.host_picker.is_some() || self.target_tab.key_picker.is_some();
        let detail_open = self.catalog_tab.detail.is_some();

        match key.code {
            KeyCode::Esc if !pickers_open && !detail_open => return true,
            KeyCode::F(1) if !pickers_open && !detail_open => {
                self.tab = Tab::Target;
                return false;
            }
            KeyCode::F(2) if !pickers_open && !detail_open => {
                self.tab = Tab::Catalog;
                return false;
            }
            KeyCode::F(3) if !pickers_open && !detail_open => {
                self.tab = Tab::Review;
                return false;
            }
            KeyCode::F(4) if !pickers_open && !detail_open => {
                self.tab = Tab::Revert;
                return false;
            }
            _ => {}
        }

        match self.tab {
            Tab::Target => self.handle_target_key(key),
            Tab::Catalog => self.handle_catalog_key(key),
            Tab::Review => self.handle_review_key(key),
            Tab::Revert => self.handle_revert_key(key),
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

        if self.target_tab.field == TargetField::Mode {
            match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Enter => {
                    self.target_tab.is_remote = !self.target_tab.is_remote;
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
            let result = (|| -> Result<(TargetInfo, HashMap<String, String>), String> {
                let session = ExecSession::open(&target)?;
                let info = engine::probe_target(&session)?;
                let tunables: Vec<&'static Tunable> = catalog::CATALOG.iter().collect();
                let values = engine::read_all_values(&session, &tunables);
                Ok((info, values))
            })();
            match result {
                Ok((info, values)) => {
                    let _ = tx.send(Msg::Connected { label, info, values });
                }
                Err(e) => {
                    let _ = tx.send(Msg::ConnectFailed(e));
                }
            }
        });
    }

    fn handle_catalog_key(&mut self, key: KeyEvent) {
        if self.catalog_tab.detail.is_some() {
            self.handle_detail_modal_key(key);
            return;
        }

        let filtered_len = self.catalog_tab.filtered().len();
        if self.catalog_tab.field == CatalogField::Table && filtered_len > 0 {
            match key.code {
                KeyCode::Up => {
                    if self.catalog_tab.selected_row > 0 {
                        self.catalog_tab.selected_row -= 1;
                    }
                    return;
                }
                KeyCode::Down => {
                    if self.catalog_tab.selected_row + 1 < filtered_len {
                        self.catalog_tab.selected_row += 1;
                    }
                    return;
                }
                KeyCode::Enter => {
                    self.open_detail_for_selected();
                    return;
                }
                _ => {}
            }
        }
        if self.catalog_tab.field == CatalogField::ProfileFilter {
            match key.code {
                KeyCode::Left => {
                    self.catalog_tab.cycle_profile(-1);
                    return;
                }
                KeyCode::Right => {
                    self.catalog_tab.cycle_profile(1);
                    return;
                }
                _ => {}
            }
        }
        if self.catalog_tab.field == CatalogField::CategoryFilter {
            match key.code {
                KeyCode::Left => {
                    self.catalog_tab.cycle_category(-1);
                    return;
                }
                KeyCode::Right => {
                    self.catalog_tab.cycle_category(1);
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Tab => self.catalog_tab.next_field(),
            KeyCode::BackTab => self.catalog_tab.prev_field(),
            KeyCode::Up => self.catalog_tab.prev_field(),
            KeyCode::Down => self.catalog_tab.next_field(),
            KeyCode::Enter if self.catalog_tab.field == CatalogField::BtnApplyProfile => self.trigger_apply_profile(),
            _ => {}
        }
    }

    fn open_detail_for_selected(&mut self) {
        let filtered = self.catalog_tab.filtered();
        if let Some(t) = filtered.get(self.catalog_tab.selected_row).copied() {
            let current = self.current_values.get(t.key).map(|s| s.as_str());
            let staged = self.staged.iter().find(|s| s.key == t.key).map(|s| s.value.as_str());
            self.catalog_tab.detail = Some(DetailModal::open(t, current, staged, self.catalog_tab.profile_filter));
        }
    }

    fn handle_detail_modal_key(&mut self, key: KeyEvent) {
        let Some(mut m) = self.catalog_tab.detail.take() else { return };
        let mut close = false;
        match key.code {
            KeyCode::Esc => close = true,
            KeyCode::Tab => m.next_field(),
            KeyCode::BackTab => m.prev_field(),
            KeyCode::Up => m.prev_field(),
            KeyCode::Down => m.next_field(),
            KeyCode::Enter => match m.field {
                DetailField::BtnUseRecommended => {
                    let t = catalog::by_key(m.key);
                    let rec = self.catalog_tab.profile_filter.zip(t).and_then(|(p, t)| t.recommended_for(p));
                    if let Some(rec) = rec {
                        m.value = Input::new(rec.value);
                    } else {
                        self.history.push((
                            false,
                            "No recommendation for the current Profile filter — pick one on the Catalog tab, or check the list in this dialog.".to_string(),
                        ));
                    }
                }
                DetailField::BtnStage => {
                    let value = m.value.value().trim().to_string();
                    if value.is_empty() {
                        self.error("Enter a value first");
                    } else if let Some(t) = catalog::by_key(m.key) {
                        let previous = self.current_values.get(t.key).cloned().unwrap_or_default();
                        let existing_persist = self.staged.iter().find(|s| s.key == t.key).map(|s| s.persist).unwrap_or(false);
                        self.staged.retain(|s| s.key != t.key);
                        self.staged.push(StagedChange { key: t.key, previous, value: value.clone(), persist: existing_persist });
                        self.history.push((true, format!("Staged {} = {value}", t.key)));
                    }
                    close = true;
                }
                DetailField::BtnCancel => close = true,
                _ => m.next_field(),
            },
            KeyCode::Char(c) if m.field == DetailField::Value => m.value.insert(c),
            KeyCode::Backspace if m.field == DetailField::Value => m.value.backspace(),
            KeyCode::Delete if m.field == DetailField::Value => m.value.delete(),
            KeyCode::Left if m.field == DetailField::Value => m.value.left(),
            KeyCode::Right if m.field == DetailField::Value => m.value.right(),
            KeyCode::Home if m.field == DetailField::Value => m.value.home(),
            KeyCode::End if m.field == DetailField::Value => m.value.end_of_line(),
            _ => {}
        }
        if !close {
            self.catalog_tab.detail = Some(m);
        }
    }

    fn trigger_apply_profile(&mut self) {
        let Some(p) = self.catalog_tab.profile_filter else {
            self.error("Pick a profile first (Left/Right on the Profile field)");
            return;
        };
        let mut n = 0;
        for t in catalog::CATALOG.iter() {
            if let Some(rec) = t.recommended_for(p) {
                let previous = self.current_values.get(t.key).cloned().unwrap_or_default();
                let existing_persist = self.staged.iter().find(|s| s.key == t.key).map(|s| s.persist).unwrap_or(false);
                self.staged.retain(|s| s.key != t.key);
                self.staged.push(StagedChange { key: t.key, previous, value: rec.value.to_string(), persist: existing_persist });
                n += 1;
            }
        }
        self.history.push((true, format!("Staged {n} change(s) for profile '{}' — review on the Review tab", p.label())));
        self.tab = Tab::Review;
    }

    fn handle_review_key(&mut self, key: KeyEvent) {
        let n = self.staged.len();
        if self.review_tab.field == ReviewField::Table && n > 0 {
            match key.code {
                KeyCode::Up => {
                    if self.review_tab.selected_row > 0 {
                        self.review_tab.selected_row -= 1;
                    }
                    return;
                }
                KeyCode::Down => {
                    if self.review_tab.selected_row + 1 < n {
                        self.review_tab.selected_row += 1;
                    }
                    return;
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    if let Some(s) = self.staged.get_mut(self.review_tab.selected_row) {
                        s.persist = !s.persist;
                    }
                    return;
                }
                KeyCode::Delete | KeyCode::Backspace => {
                    if self.review_tab.selected_row < self.staged.len() {
                        self.staged.remove(self.review_tab.selected_row);
                        if self.review_tab.selected_row >= self.staged.len() {
                            self.review_tab.selected_row = self.staged.len().saturating_sub(1);
                        }
                    }
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Tab => self.review_tab.next_field(),
            KeyCode::BackTab => self.review_tab.prev_field(),
            KeyCode::Enter => match self.review_tab.field {
                ReviewField::BtnPersistAll => {
                    let all_on = !self.staged.is_empty() && self.staged.iter().all(|s| s.persist);
                    for s in self.staged.iter_mut() {
                        s.persist = !all_on;
                    }
                }
                ReviewField::BtnApplyAll => self.request_apply_all(),
                ReviewField::BtnClearAll => {
                    self.staged.clear();
                    self.review_tab.selected_row = 0;
                }
                _ => self.review_tab.next_field(),
            },
            _ => {}
        }
    }

    fn request_apply_all(&mut self) {
        if self.staged.is_empty() {
            self.error("Nothing staged — pick values from the Catalog tab first");
            return;
        }
        if self.target_label.is_empty() {
            self.error("Connect to a target first (Target tab)");
            return;
        }
        let risky: Vec<String> = self
            .staged
            .iter()
            .filter_map(|s| catalog::by_key(s.key))
            .filter(|t| t.risk != Risk::Safe)
            .map(|t| format!("{} — {}", t.title, t.risk.label()))
            .collect();
        if !risky.is_empty() {
            self.confirm_apply = Some(ConfirmModal { lines: risky });
        } else {
            self.spawn_apply_all();
        }
    }

    fn spawn_apply_all(&mut self) {
        let target = self.target_tab.as_target();
        let changes = self.staged.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let session = match ExecSession::open(&target) {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(Msg::Log(false, format!("Connect failed: {}", one_line(&e))));
                    return;
                }
            };
            for change in changes {
                let Some(t) = catalog::by_key(change.key) else { continue };
                let actual_persist = change.persist || matches!(t.kind, Kind::Limits { .. });
                match engine::apply_change(&session, t, &change.value, change.persist) {
                    Ok(message) => {
                        let _ = tx.send(Msg::ApplyResult {
                            key: t.key,
                            ok: true,
                            message,
                            previous: change.previous.clone(),
                            new_value: change.value.clone(),
                            persist: actual_persist,
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::ApplyResult {
                            key: t.key,
                            ok: false,
                            message: one_line(&e),
                            previous: change.previous.clone(),
                            new_value: change.value.clone(),
                            persist: actual_persist,
                        });
                    }
                }
            }
            let _ = tx.send(Msg::ApplyDone);
        });
    }

    fn handle_revert_key(&mut self, key: KeyEvent) {
        let entries = self.filtered_history();
        let n = entries.len();
        match key.code {
            KeyCode::Up => {
                if self.revert_tab.selected_row > 0 {
                    self.revert_tab.selected_row -= 1;
                }
            }
            KeyCode::Down => {
                if self.revert_tab.selected_row + 1 < n {
                    self.revert_tab.selected_row += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(e) = entries.get(self.revert_tab.selected_row) {
                    self.trigger_revert_one(e.key.clone());
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') => self.trigger_revert_all(),
            _ => {}
        }
    }

    fn trigger_revert_one(&mut self, key: String) {
        if self.target_label.is_empty() {
            self.error("Connect to the target first");
            return;
        }
        let Some(entry) = self.history_entries.iter().find(|e| e.target == self.target_label && e.key == key).cloned() else { return };
        let Some(t) = catalog::by_key(&entry.key) else { return };
        let target = self.target_tab.as_target();
        let target_label = self.target_label.clone();
        let tx = self.tx.clone();
        let t_key = t.key;
        let previous = entry.previous_value.clone();
        let persisted = entry.persisted;
        thread::spawn(move || {
            let result = (|| -> Result<(), String> {
                let session = ExecSession::open(&target)?;
                let t = catalog::by_key(t_key).ok_or_else(|| "unknown tunable".to_string())?;
                engine::revert(&session, t, &previous, persisted)
            })();
            match result {
                Ok(()) => {
                    let _ = tx.send(Msg::RevertResult {
                        key: t_key.to_string(),
                        ok: true,
                        message: "reverted".into(),
                        target: target_label,
                        previous,
                    });
                }
                Err(e) => {
                    let _ = tx.send(Msg::RevertResult {
                        key: t_key.to_string(),
                        ok: false,
                        message: one_line(&e),
                        target: target_label,
                        previous,
                    });
                }
            }
        });
    }

    fn trigger_revert_all(&mut self) {
        if self.target_label.is_empty() {
            self.error("Connect to the target first");
            return;
        }
        let entries = self.filtered_history();
        if entries.is_empty() {
            return;
        }
        let target = self.target_tab.as_target();
        let target_label = self.target_label.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let session = match ExecSession::open(&target) {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(Msg::Log(false, format!("Connect failed: {}", one_line(&e))));
                    return;
                }
            };
            for entry in entries {
                let Some(t) = catalog::by_key(&entry.key) else { continue };
                let result = engine::revert(&session, t, &entry.previous_value, entry.persisted);
                match result {
                    Ok(()) => {
                        let _ = tx.send(Msg::RevertResult {
                            key: entry.key.clone(),
                            ok: true,
                            message: "reverted".into(),
                            target: target_label.clone(),
                            previous: entry.previous_value.clone(),
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::RevertResult {
                            key: entry.key.clone(),
                            ok: false,
                            message: one_line(&e),
                            target: target_label.clone(),
                            previous: entry.previous_value.clone(),
                        });
                    }
                }
            }
            let _ = tx.send(Msg::RevertDone);
        });
    }

    // ── Mouse handling ───────────────────────────────────────────────
    pub fn handle_mouse(&mut self, me: MouseEvent, area: Rect) {
        if self.modal.is_some() {
            if mouse::left_click(&me).is_some() {
                self.modal = None;
            }
            return;
        }
        // Risky-apply confirmation is keyboard-only on purpose — a stray
        // click can never trigger a Caution/Advanced change.
        if self.confirm_apply.is_some() {
            return;
        }
        // Detail modal is keyboard-only too, matching every other modal
        // in this app (Add/Edit User dialogs elsewhere aren't mouse-driven
        // either).
        if self.catalog_tab.detail.is_some() {
            return;
        }

        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(area);

        if let Some((x, y)) = mouse::left_click(&me) {
            if let Some(i) = mouse::label_row_hit(x, y, chunks[0], &["F1 Target", "F2 Catalog", "F3 Review", "F4 Revert"]) {
                self.tab = match i {
                    0 => Tab::Target,
                    1 => Tab::Catalog,
                    2 => Tab::Review,
                    _ => Tab::Revert,
                };
                return;
            }
        }

        match self.tab {
            Tab::Target => self.handle_target_mouse(me, chunks[1]),
            Tab::Catalog => self.handle_catalog_mouse(me, chunks[1]),
            Tab::Review => self.handle_review_mouse(me, chunks[1]),
            Tab::Revert => self.handle_revert_mouse(me, chunks[1]),
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
        let chunks =
            Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(form_height), Constraint::Min(3), Constraint::Length(8)]).split(area);

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
            self.target_tab.is_remote = i == 1;
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

    fn handle_catalog_mouse(&mut self, me: MouseEvent, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(5), Constraint::Min(6), Constraint::Length(8)]).split(area);

        if let Some(delta) = mouse::scroll_delta(&me) {
            let n = self.catalog_tab.filtered().len();
            if n > 0 && mouse::in_rect(chunks[1], me.column, me.row) {
                if delta < 0 && self.catalog_tab.selected_row > 0 {
                    self.catalog_tab.selected_row -= 1;
                } else if delta > 0 && self.catalog_tab.selected_row + 1 < n {
                    self.catalog_tab.selected_row += 1;
                }
            } else if mouse::in_rect(chunks[2], me.column, me.row) {
                self.history_scroll = if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };

        let filter_inner = mouse::block_inner(chunks[0]);
        let filter_rows =
            Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)]).split(filter_inner);

        if mouse::in_rect(filter_rows[0], x, y) {
            self.catalog_tab.field = CatalogField::ProfileFilter;
            let mid = filter_rows[0].x + filter_rows[0].width / 2;
            if x < mid {
                self.catalog_tab.cycle_profile(-1);
            } else {
                self.catalog_tab.cycle_profile(1);
            }
            return;
        }
        if mouse::in_rect(filter_rows[1], x, y) {
            self.catalog_tab.field = CatalogField::CategoryFilter;
            let mid = filter_rows[1].x + filter_rows[1].width / 2;
            if x < mid {
                self.catalog_tab.cycle_category(-1);
            } else {
                self.catalog_tab.cycle_category(1);
            }
            return;
        }
        if mouse::button_row_hit(x, y, filter_rows[2], &["Apply Profile to Staged"]).is_some() {
            self.trigger_apply_profile();
            return;
        }

        let filtered_len = self.catalog_tab.filtered().len();
        if let Some(idx) = mouse::table_row_hit(x, y, chunks[1], 1, filtered_len, self.catalog_tab.selected_row) {
            self.catalog_tab.selected_row = idx;
            self.catalog_tab.field = CatalogField::Table;
            self.open_detail_for_selected();
        }
    }

    fn handle_review_mouse(&mut self, me: MouseEvent, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(6), Constraint::Length(3), Constraint::Length(8)]).split(area);

        if let Some(delta) = mouse::scroll_delta(&me) {
            let n = self.staged.len();
            if n > 0 && mouse::in_rect(chunks[0], me.column, me.row) {
                if delta < 0 && self.review_tab.selected_row > 0 {
                    self.review_tab.selected_row -= 1;
                } else if delta > 0 && self.review_tab.selected_row + 1 < n {
                    self.review_tab.selected_row += 1;
                }
            } else if mouse::in_rect(chunks[2], me.column, me.row) {
                self.history_scroll = if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };

        if let Some(idx) = mouse::table_row_hit(x, y, chunks[0], 1, self.staged.len(), self.review_tab.selected_row) {
            self.review_tab.selected_row = idx;
            if let Some(s) = self.staged.get_mut(idx) {
                s.persist = !s.persist;
            }
            return;
        }

        let btn_inner = mouse::block_inner(chunks[1]);
        if let Some(i) = mouse::button_row_hit(x, y, btn_inner, &["Toggle Persist All", "Apply All", "Clear All"]) {
            match i {
                0 => {
                    let all_on = !self.staged.is_empty() && self.staged.iter().all(|s| s.persist);
                    for s in self.staged.iter_mut() {
                        s.persist = !all_on;
                    }
                }
                1 => self.request_apply_all(),
                _ => {
                    self.staged.clear();
                    self.review_tab.selected_row = 0;
                }
            }
        }
    }

    fn handle_revert_mouse(&mut self, me: MouseEvent, area: Rect) {
        let entries = self.filtered_history();
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(6), Constraint::Length(3), Constraint::Length(8)]).split(area);

        if let Some(delta) = mouse::scroll_delta(&me) {
            let n = entries.len();
            if n > 0 && mouse::in_rect(chunks[0], me.column, me.row) {
                if delta < 0 && self.revert_tab.selected_row > 0 {
                    self.revert_tab.selected_row -= 1;
                } else if delta > 0 && self.revert_tab.selected_row + 1 < n {
                    self.revert_tab.selected_row += 1;
                }
            } else if mouse::in_rect(chunks[2], me.column, me.row) {
                self.history_scroll = if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };

        if let Some(idx) = mouse::table_row_hit(x, y, chunks[0], 1, entries.len(), self.revert_tab.selected_row) {
            self.revert_tab.selected_row = idx;
            if let Some(e) = entries.get(idx) {
                self.trigger_revert_one(e.key.clone());
            }
            return;
        }

        let btn_inner = mouse::block_inner(chunks[1]);
        if mouse::button_row_hit(x, y, btn_inner, &["Revert All"]).is_some() {
            self.trigger_revert_all();
        }
    }

    // ── Draw ─────────────────────────────────────────────────────────
    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(area);

        let tab_bar = Line::from(vec![
            tab_span("F1 Target", self.tab == Tab::Target),
            Span::styled("  ", Style::default().bg(BG)),
            tab_span("F2 Catalog", self.tab == Tab::Catalog),
            Span::styled("  ", Style::default().bg(BG)),
            tab_span("F3 Review", self.tab == Tab::Review),
            Span::styled("  ", Style::default().bg(BG)),
            tab_span("F4 Revert", self.tab == Tab::Revert),
            Span::styled("  ", Style::default().bg(BG)),
            Span::styled("Esc back  Ctrl+C quit", Style::default().fg(FG2).bg(BG)),
        ]);
        f.render_widget(Paragraph::new(tab_bar), chunks[0]);

        match self.tab {
            Tab::Target => self.draw_target(f, chunks[1]),
            Tab::Catalog => self.draw_catalog(f, chunks[1]),
            Tab::Review => self.draw_review(f, chunks[1]),
            Tab::Revert => self.draw_revert(f, chunks[1]),
        }

        if let Some(m) = &self.confirm_apply {
            draw_confirm_modal(f, m, area);
        }
        if let Some(d) = &self.catalog_tab.detail {
            draw_detail_modal(f, d, area);
        }
        if let Some((title, msg)) = &self.modal {
            draw_modal(f, title, msg, area);
        }
    }

    fn draw_target(&self, f: &mut Frame, area: Rect) {
        let is_remote = self.target_tab.is_remote;
        let form_height = if is_remote { 20 } else { 10 };
        let chunks =
            Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(form_height), Constraint::Min(3), Constraint::Length(8)]).split(area);

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
                Line::from(vec![
                    Span::styled("Key Path   ", lbl()),
                    input_span(&tt.ssh_key_path, tt.field == TargetField::SshKeyPath, false, w(rows[8])),
                ]),
                rows[8],
            );
            f.render_widget(
                Line::from(vec![
                    Span::styled("Password   ", lbl()),
                    input_span(&tt.ssh_password, tt.field == TargetField::SshPassword, true, w(rows[10])),
                ]),
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
            let lines: Vec<Line> = if tt.connecting {
                vec![Line::from(Span::styled("Connecting…", Style::default().fg(YELLOW)))]
            } else if let Some(info) = &self.target_info {
                vec![
                    Line::from(vec![Span::styled("Connected to: ", lbl()), Span::styled(self.target_label.clone(), Style::default().fg(GREEN))]),
                    Line::from(vec![Span::styled("OS: ", lbl()), Span::raw(info.os_pretty.clone())]),
                    Line::from(vec![
                        Span::styled("Kernel: ", lbl()),
                        Span::raw(info.kernel.clone()),
                        Span::raw("   "),
                        Span::styled("Arch: ", lbl()),
                        Span::raw(info.arch.clone()),
                    ]),
                    Line::from(vec![
                        Span::styled("CPUs: ", lbl()),
                        Span::raw(info.cpus.clone()),
                        Span::raw("   "),
                        Span::styled("RAM: ", lbl()),
                        Span::raw(format_mem_kb(&info.mem_total_kb)),
                    ]),
                ]
            } else {
                vec![Line::from(Span::styled("Not connected yet. Choose Local or fill in a remote host above, then press Connect.", lbl()))]
            };
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), info_inner);
        }

        draw_history(f, &self.history, chunks[2], self.history_scroll);
    }

    fn draw_catalog(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(5), Constraint::Min(6), Constraint::Length(8)]).split(area);

        let filter_block = theme_block(" Filters — Left/Right cycles, Tab moves between fields ");
        let filter_inner = filter_block.inner(chunks[0]);
        f.render_widget(filter_block, chunks[0]);
        let filter_rows =
            Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)]).split(filter_inner);

        let ct = &self.catalog_tab;
        let profile_label = ct.profile_filter.map(|p| p.label()).unwrap_or("All");
        f.render_widget(
            Line::from(vec![
                Span::styled(if ct.field == CatalogField::ProfileFilter { "> " } else { "  " }, Style::default().fg(ACCENT)),
                Span::styled("Profile:  ", lbl()),
                Span::styled(format!("‹ {profile_label} ›"), Style::default().fg(FG).add_modifier(Modifier::BOLD)),
                Span::styled("   picking one filters the table and lets 'Apply Profile' bulk-stage its recommendations", lbl()),
            ]),
            filter_rows[0],
        );
        let category_label = ct.category_filter.map(|c| c.label()).unwrap_or("All");
        f.render_widget(
            Line::from(vec![
                Span::styled(if ct.field == CatalogField::CategoryFilter { "> " } else { "  " }, Style::default().fg(ACCENT)),
                Span::styled("Category: ", lbl()),
                Span::styled(format!("‹ {category_label} ›"), Style::default().fg(FG).add_modifier(Modifier::BOLD)),
            ]),
            filter_rows[1],
        );
        f.render_widget(Line::from(vec![btn_span("Apply Profile to Staged", ct.field == CatalogField::BtnApplyProfile)]), filter_rows[2]);

        let filtered = ct.filtered();
        let header = Row::new(vec![
            Cell::from(Span::styled("Tunable", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Current", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Staged", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Risk", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
        ])
        .style(Style::default().bg(BG2));

        let rows: Vec<Row> = filtered
            .iter()
            .map(|t| {
                let current = self.current_values.get(t.key).map(|s| s.as_str()).unwrap_or("n/a");
                let staged = self.staged.iter().find(|s| s.key == t.key).map(|s| s.value.clone()).unwrap_or_default();
                let risk_color = risk_color(t.risk);
                Row::new(vec![
                    Cell::from(t.title),
                    Cell::from(current.to_string()),
                    Cell::from(Span::styled(staged, Style::default().fg(ACCENT))),
                    Cell::from(Span::styled(t.risk.label(), Style::default().fg(risk_color))),
                ])
            })
            .collect();

        let table = Table::new(rows, [Constraint::Length(32), Constraint::Length(24), Constraint::Length(24), Constraint::Length(10)])
            .header(header)
            .block(theme_block(" Catalog — Enter opens details & hints "))
            .row_highlight_style(if ct.field == CatalogField::Table { focused() } else { normal() })
            .highlight_symbol(" \u{25B6} ")
            .style(Style::default().fg(FG).bg(BG));
        let mut tstate = TableState::default();
        if !filtered.is_empty() {
            tstate.select(Some(ct.selected_row.min(filtered.len() - 1)));
        }
        f.render_stateful_widget(table, chunks[1], &mut tstate);

        draw_history(f, &self.history, chunks[2], self.history_scroll);
    }

    fn draw_review(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(6), Constraint::Length(3), Constraint::Length(8)]).split(area);

        let header = Row::new(vec![
            Cell::from(Span::styled("Tunable", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Current", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("New", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Persist?", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Risk", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
        ])
        .style(Style::default().bg(BG2));

        let rows: Vec<Row> = self
            .staged
            .iter()
            .map(|s| {
                let t = catalog::by_key(s.key);
                let title = t.map(|t| t.title).unwrap_or(s.key);
                let risk = t.map(|t| t.risk).unwrap_or(Risk::Safe);
                let is_limits = t.map(|t| matches!(t.kind, Kind::Limits { .. })).unwrap_or(false);
                let persist_cell = if is_limits {
                    Span::styled("always (no runtime-only form)", Style::default().fg(FG2))
                } else if s.persist {
                    Span::styled("[x] yes — survives reboot", Style::default().fg(YELLOW))
                } else {
                    Span::styled("[ ] no — this session only", Style::default().fg(FG2))
                };
                Row::new(vec![
                    Cell::from(title),
                    Cell::from(s.previous.clone()),
                    Cell::from(Span::styled(s.value.clone(), Style::default().fg(ACCENT))),
                    Cell::from(persist_cell),
                    Cell::from(Span::styled(risk.label(), Style::default().fg(risk_color(risk)))),
                ])
            })
            .collect();

        let table = Table::new(rows, [Constraint::Length(26), Constraint::Length(22), Constraint::Length(22), Constraint::Length(30), Constraint::Length(10)])
            .header(header)
            .block(theme_block(" Staged Changes — Enter/Space toggles Persist, Delete removes a row "))
            .row_highlight_style(if self.review_tab.field == ReviewField::Table { focused() } else { normal() })
            .highlight_symbol(" \u{25B6} ")
            .style(Style::default().fg(FG).bg(BG));
        let mut tstate = TableState::default();
        if !self.staged.is_empty() {
            tstate.select(Some(self.review_tab.selected_row.min(self.staged.len() - 1)));
        }
        f.render_stateful_widget(table, chunks[0], &mut tstate);

        let btn_block = theme_block("");
        let btn_inner = btn_block.inner(chunks[1]);
        f.render_widget(btn_block, chunks[1]);
        f.render_widget(
            Line::from(vec![
                btn_span("Toggle Persist All", self.review_tab.field == ReviewField::BtnPersistAll),
                Span::raw("  "),
                btn_span("Apply All", self.review_tab.field == ReviewField::BtnApplyAll),
                Span::raw("  "),
                btn_span("Clear All", self.review_tab.field == ReviewField::BtnClearAll),
            ]),
            btn_inner,
        );

        draw_history(f, &self.history, chunks[2], self.history_scroll);
    }

    fn draw_revert(&self, f: &mut Frame, area: Rect) {
        let entries = self.filtered_history();
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(6), Constraint::Length(3), Constraint::Length(8)]).split(area);

        let header = Row::new(vec![
            Cell::from(Span::styled("Tunable", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Previous", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Applied", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Persisted", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("When", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
        ])
        .style(Style::default().bg(BG2));

        let rows: Vec<Row> = entries
            .iter()
            .map(|e| {
                Row::new(vec![
                    Cell::from(e.title.clone()),
                    Cell::from(e.previous_value.clone()),
                    Cell::from(e.new_value.clone()),
                    Cell::from(if e.persisted { "yes" } else { "no" }),
                    Cell::from(relative_time(&e.when)),
                ])
            })
            .collect();

        let title = if self.target_label.is_empty() {
            " Revert — connect to a target on the Target tab to see its history ".to_string()
        } else {
            format!(" Revert — changes atk applied to {} (Enter reverts the selected row) ", self.target_label)
        };
        let table = Table::new(rows, [Constraint::Length(28), Constraint::Length(16), Constraint::Length(16), Constraint::Length(10), Constraint::Length(12)])
            .header(header)
            .block(theme_block(&title))
            .row_highlight_style(focused())
            .highlight_symbol(" \u{25B6} ")
            .style(Style::default().fg(FG).bg(BG));
        let mut tstate = TableState::default();
        if !entries.is_empty() {
            tstate.select(Some(self.revert_tab.selected_row.min(entries.len() - 1)));
        }
        f.render_stateful_widget(table, chunks[0], &mut tstate);

        let btn_block = theme_block("");
        let btn_inner = btn_block.inner(chunks[1]);
        f.render_widget(btn_block, chunks[1]);
        f.render_widget(Line::from(vec![btn_span("Revert All (A)", false)]), btn_inner);

        draw_history(f, &self.history, chunks[2], self.history_scroll);
    }
}

fn risk_color(r: Risk) -> ratatui::style::Color {
    match r {
        Risk::Safe => GREEN,
        Risk::Caution => YELLOW,
        Risk::Advanced => RED,
    }
}

fn format_mem_kb(kb: &str) -> String {
    match kb.trim().parse::<u64>() {
        Ok(kb) => {
            let gb = kb as f64 / 1024.0 / 1024.0;
            if gb >= 1.0 {
                format!("{gb:.1} GB")
            } else {
                format!("{} MB", kb / 1024)
            }
        }
        Err(_) => "unknown".to_string(),
    }
}

fn relative_time(epoch_secs: &str) -> String {
    let Ok(then) = epoch_secs.trim().parse::<u64>() else { return epoch_secs.to_string() };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let diff = now.saturating_sub(then);
    if diff < 60 {
        format!("{diff}s ago")
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else {
        format!("{}d ago", diff / 86400)
    }
}

fn draw_detail_modal(f: &mut Frame, m: &DetailModal, area: Rect) {
    let Some(t) = catalog::by_key(m.key) else { return };
    let width = 90u16.min(area.width.saturating_sub(4));
    let height = 22u16.min(area.height.saturating_sub(2));
    let modal_area = centered_rect(width, height, area);
    f.render_widget(Clear, modal_area);
    let block = Block::default()
        .title(Span::styled(format!(" {} ", t.title), Style::default().fg(TITLE)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(BG2));
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // key / risk / default
            Constraint::Length(1), // spacer
            Constraint::Min(2),    // description
            Constraint::Length(1), // spacer
            Constraint::Min(4),    // why
            Constraint::Length(1), // spacer
            Constraint::Min(4),    // recommended-per-profile (header + up to a few entries, one per line)
            Constraint::Length(1), // spacer
            Constraint::Length(1), // value input
            Constraint::Length(1), // spacer
            Constraint::Length(1), // buttons
        ])
        .split(inner);

    f.render_widget(
        Line::from(vec![
            Span::styled(t.key, Style::default().fg(FG2)),
            Span::raw("   "),
            Span::styled(t.risk.label(), Style::default().fg(risk_color(t.risk)).add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled(format!("typical default: {}", t.default_hint), lbl()),
        ]),
        rows[0],
    );

    f.render_widget(Paragraph::new(t.description).wrap(Wrap { trim: true }).style(Style::default().fg(FG)), rows[2]);
    f.render_widget(
        Paragraph::new(vec![Line::from(Span::styled("Why: ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))), Line::from(t.why)])
            .wrap(Wrap { trim: true }),
        rows[4],
    );

    // One `Line` per profile entry — joining into a single `Line` with `\n`
    // separators doesn't work in ratatui: a `Line` renders its string as
    // one run of text, so the embedded newlines aren't treated as line
    // breaks and just vanish under `Wrap`, running two profiles' text
    // together (e.g. "...upstreams.Gaming Server: ...", no separator).
    let mut rec_lines: Vec<Line> =
        vec![Line::from(Span::styled("Recommended by scenario:", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)))];
    if t.profiles.is_empty() {
        rec_lines.push(Line::from("No scenario-specific recommendation in the catalog — set a value based on your own workload."));
    } else {
        for pv in t.profiles {
            let line = if pv.note.is_empty() {
                format!("{}: {}", pv.profile.label(), pv.value)
            } else {
                format!("{}: {} — {}", pv.profile.label(), pv.value, pv.note)
            };
            rec_lines.push(Line::from(line));
        }
    }
    f.render_widget(Paragraph::new(rec_lines).wrap(Wrap { trim: true }), rows[6]);

    let value_w = (rows[8].width as usize).saturating_sub(8).max(10);
    f.render_widget(Line::from(vec![Span::styled("Value: ", lbl()), input_span(&m.value, m.field == DetailField::Value, false, value_w)]), rows[8]);

    f.render_widget(
        Line::from(vec![
            btn_span("Use Recommended", m.field == DetailField::BtnUseRecommended),
            Span::raw("  "),
            btn_span("Stage", m.field == DetailField::BtnStage),
            Span::raw("  "),
            btn_span("Cancel", m.field == DetailField::BtnCancel),
        ]),
        rows[10],
    );
}

fn draw_confirm_modal(f: &mut Frame, m: &ConfirmModal, area: Rect) {
    let mut msg = String::from("This apply includes change(s) flagged Caution/Advanced:\n\n");
    for l in &m.lines {
        msg.push_str("  • ");
        msg.push_str(l);
        msg.push('\n');
    }
    msg.push_str("\nEnter to apply anyway, Esc to cancel. (Keyboard only — a stray click can't confirm this.)");
    let width = 74u16.min(area.width.saturating_sub(4));
    let height = (m.lines.len() as u16 + 7).min(area.height.saturating_sub(2)).max(8);
    let modal_area = centered_rect(width, height, area);
    f.render_widget(Clear, modal_area);
    f.render_widget(
        Paragraph::new(msg)
            .block(
                Block::default()
                    .title(Span::styled(" Confirm Risky Changes ", Style::default().fg(YELLOW)))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(YELLOW)),
            )
            .style(Style::default().fg(FG).bg(BG2))
            .wrap(Wrap { trim: true }),
        modal_area,
    );
}
