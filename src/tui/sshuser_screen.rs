//! SSH User Manager screen — create/remove Linux users + authorized_keys on
//! remote hosts, manage reusable key "profiles", and default SSH settings.

use std::{sync::mpsc, thread};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::{Clear, List, ListItem, ListState, Paragraph, Row, Table, Wrap},
    Frame,
};

use crate::sshuser::{commands, config, make_creds};
use crate::ssh_exec::{self, one_line};

use super::host_picker::HostPicker;
use super::mouse;
use super::widgets::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    User,
    Profiles,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UserField {
    Servers,
    Port,
    Profile,
    BtnAdd,
    BtnRemove,
}

struct UserTab {
    servers: Input,
    port: Input,
    profile_input: Input,
    profile_idx: usize,
    dropdown_open: bool,
    field: UserField,
    history: Vec<(bool, String)>,
    host_picker: Option<HostPicker>,
}

impl UserTab {
    fn new(cfg: &config::Config) -> Self {
        Self {
            servers: Input::default(),
            port: Input::new(&cfg.default_port),
            profile_input: Input::default(),
            profile_idx: 0,
            dropdown_open: false,
            field: UserField::Servers,
            history: Vec::new(),
            host_picker: None,
        }
    }

    /// Appends a host picked from the SSH Server Manager to the (possibly
    /// already comma-separated) Servers field, rather than replacing it —
    /// this field is a bulk list, so picking a known host should add to it
    /// the same way pasting another IP would.
    fn append_host(&mut self, server: &crate::easyssh_mgr::config::Server) {
        let existing = self.servers.value().trim();
        let host = server.effective_host();
        let new_val = if existing.is_empty() { host.to_string() } else { format!("{existing}, {host}") };
        self.servers = Input::new(&new_val);
    }

    fn next_field(&mut self) {
        self.field = match self.field {
            UserField::Servers => UserField::Port,
            UserField::Port => UserField::Profile,
            UserField::Profile => UserField::BtnAdd,
            UserField::BtnAdd => UserField::BtnRemove,
            UserField::BtnRemove => UserField::Servers,
        };
    }

    fn prev_field(&mut self) {
        self.field = match self.field {
            UserField::Servers => UserField::BtnRemove,
            UserField::Port => UserField::Servers,
            UserField::Profile => UserField::Port,
            UserField::BtnAdd => UserField::Profile,
            UserField::BtnRemove => UserField::BtnAdd,
        };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProfilesField {
    Name,
    Key,
    BtnAdd,
    DelName,
    BtnDel,
}

struct ProfilesTab {
    name: Input,
    key: Input,
    del_name: Input,
    field: ProfilesField,
}

impl ProfilesTab {
    fn new() -> Self {
        Self {
            name: Input::default(),
            key: Input::default(),
            del_name: Input::default(),
            field: ProfilesField::Name,
        }
    }

    fn next_field(&mut self) {
        self.field = match self.field {
            ProfilesField::Name => ProfilesField::Key,
            ProfilesField::Key => ProfilesField::BtnAdd,
            ProfilesField::BtnAdd => ProfilesField::DelName,
            ProfilesField::DelName => ProfilesField::BtnDel,
            ProfilesField::BtnDel => ProfilesField::Name,
        };
    }

    fn prev_field(&mut self) {
        self.field = match self.field {
            ProfilesField::Name => ProfilesField::BtnDel,
            ProfilesField::Key => ProfilesField::Name,
            ProfilesField::BtnAdd => ProfilesField::Key,
            ProfilesField::DelName => ProfilesField::BtnAdd,
            ProfilesField::BtnDel => ProfilesField::DelName,
        };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsField {
    SshUser,
    SshKeyPath,
    SshPassword,
    Port,
    BtnSave,
}

struct SettingsTab {
    ssh_user: Input,
    ssh_key_path: Input,
    ssh_password: Input,
    port: Input,
    field: SettingsField,
}

impl SettingsTab {
    fn new(cfg: &config::Config) -> Self {
        Self {
            ssh_user: Input::new(&cfg.default_ssh_user),
            ssh_key_path: Input::new(&cfg.default_ssh_key_path),
            ssh_password: Input::new(&cfg.default_ssh_password),
            port: Input::new(&cfg.default_port),
            field: SettingsField::SshUser,
        }
    }

    fn next_field(&mut self) {
        self.field = match self.field {
            SettingsField::SshUser => SettingsField::SshKeyPath,
            SettingsField::SshKeyPath => SettingsField::SshPassword,
            SettingsField::SshPassword => SettingsField::Port,
            SettingsField::Port => SettingsField::BtnSave,
            SettingsField::BtnSave => SettingsField::SshUser,
        };
    }

    fn prev_field(&mut self) {
        self.field = match self.field {
            SettingsField::SshUser => SettingsField::BtnSave,
            SettingsField::SshKeyPath => SettingsField::SshUser,
            SettingsField::SshPassword => SettingsField::SshKeyPath,
            SettingsField::Port => SettingsField::SshPassword,
            SettingsField::BtnSave => SettingsField::Port,
        };
    }
}

pub struct SshUserScreen {
    tab: Tab,
    user_tab: UserTab,
    profiles_tab: ProfilesTab,
    settings_tab: SettingsTab,
    modal: Option<(String, String)>,
    cfg: config::Config,
    result_tx: mpsc::Sender<(bool, String)>,
    result_rx: mpsc::Receiver<(bool, String)>,
}

impl SshUserScreen {
    pub fn new() -> Self {
        let cfg = config::load().unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        Self {
            tab: Tab::User,
            user_tab: UserTab::new(&cfg),
            profiles_tab: ProfilesTab::new(),
            settings_tab: SettingsTab::new(&cfg),
            modal: None,
            cfg,
            result_tx: tx,
            result_rx: rx,
        }
    }

    pub fn tick(&mut self) {
        while let Ok(item) = self.result_rx.try_recv() {
            self.user_tab.history.push(item);
        }
    }

    /// Returns true if the screen wants to go back to the home menu.
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

        let picker_closed = self.user_tab.host_picker.is_none();
        match key.code {
            KeyCode::Esc if !self.user_tab.dropdown_open && picker_closed => return true,
            KeyCode::F(1) if picker_closed => {
                self.tab = Tab::User;
                return false;
            }
            KeyCode::F(2) if picker_closed => {
                self.tab = Tab::Profiles;
                return false;
            }
            KeyCode::F(3) if picker_closed => {
                self.tab = Tab::Settings;
                return false;
            }
            _ => {}
        }

        match self.tab {
            Tab::User => self.handle_user_key(key),
            Tab::Profiles => self.handle_profiles_key(key),
            Tab::Settings => self.handle_settings_key(key),
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
            if let Some(i) = mouse::label_row_hit(x, y, chunks[0], &["F1 User", "F2 Profiles", "F3 Settings"]) {
                self.tab = match i {
                    0 => Tab::User,
                    1 => Tab::Profiles,
                    _ => Tab::Settings,
                };
                return;
            }
        }

        match self.tab {
            Tab::User => self.handle_user_mouse(me, chunks[1]),
            Tab::Profiles => self.handle_profiles_mouse(me, chunks[1]),
            Tab::Settings => self.handle_settings_mouse(me, chunks[1]),
        }
    }

    fn handle_user_mouse(&mut self, me: MouseEvent, area: Rect) {
        if let Some(picker) = &self.user_tab.host_picker {
            if let Some((x, y)) = mouse::left_click(&me) {
                if let Some(idx) = picker.row_at(area, x, y) {
                    self.user_tab.host_picker.as_mut().unwrap().selected = idx;
                    if let Some(server) = self.user_tab.host_picker.as_ref().unwrap().activate() {
                        self.user_tab.append_host(&server);
                        self.user_tab.host_picker = None;
                    }
                }
                return;
            }
            if let Some(delta) = mouse::scroll_delta(&me) {
                let p = self.user_tab.host_picker.as_mut().unwrap();
                if delta < 0 {
                    p.up();
                } else {
                    p.down();
                }
            }
            return;
        }

        if self.user_tab.dropdown_open {
            return;
        }

        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(13), Constraint::Min(0)]).split(area);
        let form_inner = mouse::block_inner(chunks[0]);
        let form_rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // 0 Servers
                Constraint::Length(1),
                Constraint::Length(1), // 2 Port
                Constraint::Length(1),
                Constraint::Length(1), // 4 Profile
                Constraint::Length(1),
                Constraint::Length(1), // 6 buttons
            ])
            .split(form_inner);

        let Some((x, y)) = mouse::left_click(&me) else { return };

        if let Some(i) = mouse::button_row_hit(x, y, form_rows[6], &["Add User", "Remove User"]) {
            self.trigger_user_action(i == 1);
            return;
        }
        if mouse::in_rect(form_rows[0], x, y) {
            self.user_tab.field = UserField::Servers;
        } else if mouse::in_rect(form_rows[2], x, y) {
            self.user_tab.field = UserField::Port;
        } else if mouse::in_rect(form_rows[4], x, y) {
            self.user_tab.field = UserField::Profile;
            if !self.cfg.profiles.is_empty() {
                self.user_tab.profile_idx = 0;
                self.user_tab.dropdown_open = true;
            }
        }
    }

    fn handle_profiles_mouse(&mut self, me: MouseEvent, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(0), Constraint::Length(11)]).split(area);

        let Some((x, y)) = mouse::left_click(&me) else { return };

        let form_inner = Rect { x: chunks[1].x, y: chunks[1].y + 1, width: chunks[1].width, height: chunks[1].height.saturating_sub(1) };
        let form_rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // 0 Name
                Constraint::Length(1),
                Constraint::Length(1), // 2 Key
                Constraint::Length(1),
                Constraint::Length(1), // 4 Add button
                Constraint::Length(1),
                Constraint::Length(1), // 6 Delete name + Delete button
            ])
            .split(form_inner);

        let label_w: u16 = 14;
        let del_fw = form_rows[6].width.saturating_sub(label_w + 2 + 10);
        let del_btn_x = form_rows[6].x + label_w + del_fw + 2;
        let clicked_del_btn = y == form_rows[6].y && x >= del_btn_x;

        if mouse::button_row_hit(x, y, form_rows[4], &["Add"]).is_some() {
            self.trigger_add_profile();
        } else if clicked_del_btn {
            self.trigger_delete_profile();
        } else if mouse::in_rect(form_rows[0], x, y) {
            self.profiles_tab.field = ProfilesField::Name;
        } else if mouse::in_rect(form_rows[2], x, y) {
            self.profiles_tab.field = ProfilesField::Key;
        } else if mouse::in_rect(form_rows[6], x, y) {
            self.profiles_tab.field = ProfilesField::DelName;
        }
    }

    fn handle_settings_mouse(&mut self, me: MouseEvent, area: Rect) {
        let inner = mouse::block_inner(area);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // 0 SSH User
                Constraint::Length(1),
                Constraint::Length(1), // 2 SSH Key Path
                Constraint::Length(1),
                Constraint::Length(1), // 4 SSH Password
                Constraint::Length(1),
                Constraint::Length(1), // 6 Port
                Constraint::Length(1),
                Constraint::Length(1), // 8 Save button
            ])
            .split(inner);

        let Some((x, y)) = mouse::left_click(&me) else { return };

        if mouse::button_row_hit(x, y, rows[8], &["Save"]).is_some() {
            self.trigger_save_settings();
        } else if mouse::in_rect(rows[0], x, y) {
            self.settings_tab.field = SettingsField::SshUser;
        } else if mouse::in_rect(rows[2], x, y) {
            self.settings_tab.field = SettingsField::SshKeyPath;
        } else if mouse::in_rect(rows[4], x, y) {
            self.settings_tab.field = SettingsField::SshPassword;
        } else if mouse::in_rect(rows[6], x, y) {
            self.settings_tab.field = SettingsField::Port;
        }
    }

    fn handle_user_key(&mut self, key: KeyEvent) {
        let ut = &mut self.user_tab;

        if ut.host_picker.is_some() {
            match key.code {
                KeyCode::Esc => ut.host_picker = None,
                KeyCode::Up => {
                    if let Some(p) = ut.host_picker.as_mut() {
                        p.up();
                    }
                }
                KeyCode::Down => {
                    if let Some(p) = ut.host_picker.as_mut() {
                        p.down();
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(p) = ut.host_picker.as_mut() {
                        p.insert(c);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(p) = ut.host_picker.as_mut() {
                        p.backspace();
                    }
                }
                KeyCode::Enter => {
                    let picked = ut.host_picker.as_ref().and_then(|p| p.activate());
                    if let Some(server) = picked {
                        ut.append_host(&server);
                        ut.host_picker = None;
                    }
                }
                _ => {}
            }
            return;
        }

        if ut.dropdown_open {
            match key.code {
                KeyCode::Esc => ut.dropdown_open = false,
                KeyCode::Tab => {
                    ut.dropdown_open = false;
                    ut.next_field();
                }
                KeyCode::BackTab => {
                    ut.dropdown_open = false;
                    ut.prev_field();
                }
                KeyCode::Up => {
                    if ut.profile_idx > 0 {
                        ut.profile_idx -= 1;
                    }
                }
                KeyCode::Down => {
                    let count = filtered_profiles(&self.cfg.profiles, ut.profile_input.value()).len();
                    if ut.profile_idx + 1 < count {
                        ut.profile_idx += 1;
                    }
                }
                KeyCode::Enter => {
                    let matches = filtered_profiles(&self.cfg.profiles, ut.profile_input.value());
                    if let Some(&name) = matches.get(ut.profile_idx) {
                        ut.profile_input = Input::new(name);
                    }
                    ut.dropdown_open = false;
                }
                KeyCode::Char(c) => {
                    ut.profile_input.insert(c);
                    ut.profile_idx = 0;
                    let count = filtered_profiles(&self.cfg.profiles, ut.profile_input.value()).len();
                    if count == 0 {
                        ut.dropdown_open = false;
                    }
                }
                KeyCode::Backspace => {
                    ut.profile_input.backspace();
                    ut.profile_idx = 0;
                }
                KeyCode::Delete => {
                    ut.profile_input.delete();
                    ut.profile_idx = 0;
                }
                KeyCode::Left => ut.profile_input.left(),
                KeyCode::Right => ut.profile_input.right(),
                KeyCode::Home => ut.profile_input.home(),
                KeyCode::End => ut.profile_input.end_of_line(),
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Tab => ut.next_field(),
            KeyCode::BackTab => ut.prev_field(),
            KeyCode::Up => ut.prev_field(),
            KeyCode::Down => ut.next_field(),
            KeyCode::Enter => match ut.field {
                UserField::BtnAdd => self.trigger_user_action(false),
                UserField::BtnRemove => self.trigger_user_action(true),
                UserField::Servers => ut.host_picker = Some(HostPicker::new()),
                UserField::Profile => {
                    if !self.cfg.profiles.is_empty() {
                        self.user_tab.profile_idx = 0;
                        self.user_tab.dropdown_open = true;
                    }
                }
                _ => self.user_tab.next_field(),
            },
            KeyCode::Char(c) => match ut.field {
                UserField::Servers => ut.servers.insert(c),
                UserField::Port => ut.port.insert(c),
                UserField::Profile => {
                    ut.profile_input.insert(c);
                    ut.profile_idx = 0;
                    if !self.cfg.profiles.is_empty() {
                        self.user_tab.dropdown_open = true;
                    }
                }
                _ => {}
            },
            KeyCode::Backspace => match ut.field {
                UserField::Servers => ut.servers.backspace(),
                UserField::Port => ut.port.backspace(),
                UserField::Profile => {
                    ut.profile_input.backspace();
                    ut.profile_idx = 0;
                }
                _ => {}
            },
            KeyCode::Delete => match ut.field {
                UserField::Servers => ut.servers.delete(),
                UserField::Port => ut.port.delete(),
                UserField::Profile => ut.profile_input.delete(),
                _ => {}
            },
            KeyCode::Left => match ut.field {
                UserField::Servers => ut.servers.left(),
                UserField::Port => ut.port.left(),
                UserField::Profile => ut.profile_input.left(),
                _ => {}
            },
            KeyCode::Right => match ut.field {
                UserField::Servers => ut.servers.right(),
                UserField::Port => ut.port.right(),
                UserField::Profile => ut.profile_input.right(),
                _ => {}
            },
            KeyCode::Home => match ut.field {
                UserField::Servers => ut.servers.home(),
                UserField::Port => ut.port.home(),
                UserField::Profile => ut.profile_input.home(),
                _ => {}
            },
            KeyCode::End => match ut.field {
                UserField::Servers => ut.servers.end_of_line(),
                UserField::Port => ut.port.end_of_line(),
                UserField::Profile => ut.profile_input.end_of_line(),
                _ => {}
            },
            _ => {}
        }
    }

    fn trigger_user_action(&mut self, remove: bool) {
        let hosts = commands::parse_server_list(self.user_tab.servers.value());
        if hosts.is_empty() {
            self.modal = Some(("Error".into(), "Enter at least one server".into()));
            return;
        }

        let prof_name = self.user_tab.profile_input.value().trim().to_string();
        let Some(prof) = self.cfg.profiles.iter().find(|p| p.name == prof_name).cloned() else {
            self.modal = Some(("Error".into(), "Select a valid profile".into()));
            return;
        };
        let port_str = self.user_tab.port.value().trim().to_string();
        let port = if port_str.is_empty() {
            self.cfg.default_port.clone()
        } else {
            port_str
        };
        let creds = make_creds(&self.cfg);
        let commands = if remove {
            commands::remove_user_commands(&prof.name)
        } else {
            commands::add_user_commands(&prof.name, &prof.key)
        };

        for host in hosts {
            let tx = self.result_tx.clone();
            let creds = creds.clone();
            let commands = commands.clone();
            let port = port.clone();
            let prof_name = prof.name.clone();
            thread::spawn(move || {
                let (ok, line) = match ssh_exec::run_commands(&host, &port, &creds, &commands) {
                    Ok((_, stderr)) => {
                        let verb = if remove { "removed" } else { "added" };
                        let warning = one_line(&stderr);
                        if warning.is_empty() {
                            (true, format!("[{host}] user {} {verb}: ok", prof_name))
                        } else {
                            (
                                true,
                                format!("[{host}] user {} {verb}: ok (warning: {})", prof_name, warning),
                            )
                        }
                    }
                    Err(e) => {
                        let verb = if remove { "not removed" } else { "not added" };
                        (false, format!("[{host}] user {} {verb}: {}", prof_name, one_line(&e)))
                    }
                };
                let _ = tx.send((ok, line));
            });
        }
    }

    fn handle_profiles_key(&mut self, key: KeyEvent) {
        let pt = &mut self.profiles_tab;
        match key.code {
            KeyCode::Tab => pt.next_field(),
            KeyCode::BackTab => pt.prev_field(),
            KeyCode::Up => pt.prev_field(),
            KeyCode::Down => pt.next_field(),
            KeyCode::Enter => match pt.field {
                ProfilesField::BtnAdd => self.trigger_add_profile(),
                ProfilesField::BtnDel => self.trigger_delete_profile(),
                _ => self.profiles_tab.next_field(),
            },
            KeyCode::Char(c) => match pt.field {
                ProfilesField::Name => pt.name.insert(c),
                ProfilesField::Key => pt.key.insert(c),
                ProfilesField::DelName => pt.del_name.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match pt.field {
                ProfilesField::Name => pt.name.backspace(),
                ProfilesField::Key => pt.key.backspace(),
                ProfilesField::DelName => pt.del_name.backspace(),
                _ => {}
            },
            KeyCode::Delete => match pt.field {
                ProfilesField::Name => pt.name.delete(),
                ProfilesField::Key => pt.key.delete(),
                ProfilesField::DelName => pt.del_name.delete(),
                _ => {}
            },
            KeyCode::Left => match pt.field {
                ProfilesField::Name => pt.name.left(),
                ProfilesField::Key => pt.key.left(),
                ProfilesField::DelName => pt.del_name.left(),
                _ => {}
            },
            KeyCode::Right => match pt.field {
                ProfilesField::Name => pt.name.right(),
                ProfilesField::Key => pt.key.right(),
                ProfilesField::DelName => pt.del_name.right(),
                _ => {}
            },
            KeyCode::Home => match pt.field {
                ProfilesField::Name => pt.name.home(),
                ProfilesField::Key => pt.key.home(),
                ProfilesField::DelName => pt.del_name.home(),
                _ => {}
            },
            KeyCode::End => match pt.field {
                ProfilesField::Name => pt.name.end_of_line(),
                ProfilesField::Key => pt.key.end_of_line(),
                ProfilesField::DelName => pt.del_name.end_of_line(),
                _ => {}
            },
            _ => {}
        }
    }

    fn trigger_add_profile(&mut self) {
        let name = self.profiles_tab.name.value().trim().to_string();
        let key = self.profiles_tab.key.value().trim().to_string();
        if name.is_empty() || key.is_empty() {
            self.modal = Some(("Error".into(), "Name and Key required".into()));
            return;
        }
        self.cfg.profiles.push(config::Profile { name, key });
        match config::save(&self.cfg) {
            Ok(_) => {
                self.profiles_tab.name = Input::default();
                self.profiles_tab.key = Input::default();
            }
            Err(e) => self.modal = Some(("Error".into(), e.to_string())),
        }
    }

    fn trigger_delete_profile(&mut self) {
        let target = self.profiles_tab.del_name.value().trim().to_string();
        self.cfg.profiles.retain(|p| p.name != target);
        match config::save(&self.cfg) {
            Ok(_) => self.profiles_tab.del_name = Input::default(),
            Err(e) => self.modal = Some(("Error".into(), e.to_string())),
        }
    }

    fn trigger_save_settings(&mut self) {
        self.cfg.default_ssh_user = self.settings_tab.ssh_user.value().to_string();
        self.cfg.default_ssh_key_path = self.settings_tab.ssh_key_path.value().to_string();
        self.cfg.default_ssh_password = self.settings_tab.ssh_password.value().to_string();
        self.cfg.default_port = self.settings_tab.port.value().to_string();
        match config::save(&self.cfg) {
            Ok(_) => self.modal = Some(("Saved".into(), "Settings saved".into())),
            Err(e) => self.modal = Some(("Error".into(), e.to_string())),
        }
    }

    fn handle_settings_key(&mut self, key: KeyEvent) {
        let st = &mut self.settings_tab;
        match key.code {
            KeyCode::Tab => st.next_field(),
            KeyCode::BackTab => st.prev_field(),
            KeyCode::Up => st.prev_field(),
            KeyCode::Down => st.next_field(),
            KeyCode::Enter => {
                if st.field == SettingsField::BtnSave {
                    self.trigger_save_settings();
                } else {
                    self.settings_tab.next_field();
                }
            }
            KeyCode::Char(c) => match st.field {
                SettingsField::SshUser => st.ssh_user.insert(c),
                SettingsField::SshKeyPath => st.ssh_key_path.insert(c),
                SettingsField::SshPassword => st.ssh_password.insert(c),
                SettingsField::Port => st.port.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match st.field {
                SettingsField::SshUser => st.ssh_user.backspace(),
                SettingsField::SshKeyPath => st.ssh_key_path.backspace(),
                SettingsField::SshPassword => st.ssh_password.backspace(),
                SettingsField::Port => st.port.backspace(),
                _ => {}
            },
            KeyCode::Delete => match st.field {
                SettingsField::SshUser => st.ssh_user.delete(),
                SettingsField::SshKeyPath => st.ssh_key_path.delete(),
                SettingsField::SshPassword => st.ssh_password.delete(),
                SettingsField::Port => st.port.delete(),
                _ => {}
            },
            KeyCode::Left => match st.field {
                SettingsField::SshUser => st.ssh_user.left(),
                SettingsField::SshKeyPath => st.ssh_key_path.left(),
                SettingsField::SshPassword => st.ssh_password.left(),
                SettingsField::Port => st.port.left(),
                _ => {}
            },
            KeyCode::Right => match st.field {
                SettingsField::SshUser => st.ssh_user.right(),
                SettingsField::SshKeyPath => st.ssh_key_path.right(),
                SettingsField::SshPassword => st.ssh_password.right(),
                SettingsField::Port => st.port.right(),
                _ => {}
            },
            KeyCode::Home => match st.field {
                SettingsField::SshUser => st.ssh_user.home(),
                SettingsField::SshKeyPath => st.ssh_key_path.home(),
                SettingsField::SshPassword => st.ssh_password.home(),
                SettingsField::Port => st.port.home(),
                _ => {}
            },
            KeyCode::End => match st.field {
                SettingsField::SshUser => st.ssh_user.end_of_line(),
                SettingsField::SshKeyPath => st.ssh_key_path.end_of_line(),
                SettingsField::SshPassword => st.ssh_password.end_of_line(),
                SettingsField::Port => st.port.end_of_line(),
                _ => {}
            },
            _ => {}
        }
    }

    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(area);

        let tab_bar = Line::from(vec![
            tab_span("F1 User", self.tab == Tab::User),
            Span::styled("  ", Style::default().bg(BG)),
            tab_span("F2 Profiles", self.tab == Tab::Profiles),
            Span::styled("  ", Style::default().bg(BG)),
            tab_span("F3 Settings", self.tab == Tab::Settings),
            Span::styled("  ", Style::default().bg(BG)),
            Span::styled("Esc back  Ctrl+C quit", Style::default().fg(FG2).bg(BG)),
        ]);
        f.render_widget(Paragraph::new(tab_bar).style(Style::default().bg(BG)), chunks[0]);

        match self.tab {
            Tab::User => self.draw_user(f, chunks[1]),
            Tab::Profiles => self.draw_profiles(f, chunks[1]),
            Tab::Settings => self.draw_settings(f, chunks[1]),
        }

        if let Some((title, msg)) = &self.modal {
            draw_modal(f, title, msg, area);
        }
    }

    fn draw_user(&self, f: &mut Frame, area: Rect) {
        let ut = &self.user_tab;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(13), Constraint::Min(0)])
            .split(area);

        let form_block = theme_block(" User ");
        let form_inner = form_block.inner(chunks[0]);
        f.render_widget(form_block, chunks[0]);

        let form_rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(form_inner);

        let label_w: u16 = 10;
        let fw = form_rows[0].width.saturating_sub(label_w) as usize;

        let servers_line = Line::from(vec![
            Span::styled("Servers:  ", lbl()),
            input_span(&ut.servers, ut.field == UserField::Servers, false, fw),
        ]);
        f.render_widget(Paragraph::new(servers_line), form_rows[0]);

        let port_line = Line::from(vec![
            Span::styled("Port:     ", lbl()),
            input_span(&ut.port, ut.field == UserField::Port, false, fw),
        ]);
        f.render_widget(Paragraph::new(port_line), form_rows[2]);

        let prof_focused = ut.field == UserField::Profile;
        let prof_line = Line::from(vec![
            Span::styled("Profile:  ", lbl()),
            input_span(&ut.profile_input, prof_focused, false, fw),
        ]);
        f.render_widget(Paragraph::new(prof_line), form_rows[4]);

        let btn_line = Line::from(vec![
            btn_span("Add User", ut.field == UserField::BtnAdd),
            Span::raw("  "),
            btn_span("Remove User", ut.field == UserField::BtnRemove),
        ]);
        f.render_widget(Paragraph::new(btn_line), form_rows[6]);

        let hint = Line::from(Span::styled(
            "Tab/\u{2191}\u{2193} navigate  Enter on Servers picks a known host  Esc back",
            lbl(),
        ));
        f.render_widget(Paragraph::new(hint), form_rows[7]);

        let history_block = theme_block(" History ");
        let history_inner = history_block.inner(chunks[1]);
        f.render_widget(history_block, chunks[1]);

        let lines: Vec<Line> = self
            .user_tab
            .history
            .iter()
            .map(|(ok, line)| {
                Line::from(Span::styled(
                    line.as_str(),
                    Style::default().fg(if *ok { GREEN } else { RED }),
                ))
            })
            .collect();

        let total = lines.len() as u16;
        let visible = history_inner.height;
        let scroll = total.saturating_sub(visible);

        f.render_widget(
            Paragraph::new(Text::from(lines))
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            history_inner,
        );

        if ut.dropdown_open {
            let matches = filtered_profiles(&self.cfg.profiles, ut.profile_input.value());
            if !matches.is_empty() {
                let items: Vec<ListItem> = matches.iter().map(|&name| ListItem::new(name)).collect();
                let list = List::new(items)
                    .block(
                        ratatui::widgets::Block::default()
                            .borders(ratatui::widgets::Borders::ALL)
                            .border_style(Style::default().fg(BORDER)),
                    )
                    .highlight_style(Style::default().fg(BG).bg(FG))
                    .style(Style::default().fg(FG).bg(BG2));

                let x = form_rows[4].x + 10;
                let y = form_rows[4].y + 1;
                let height = (matches.len() as u16 + 2).min(12);
                let width = 30u16.min(area.width.saturating_sub(x));
                let dd_area = Rect::new(x, y, width, height);

                let mut state = ListState::default();
                state.select(Some(ut.profile_idx));
                f.render_widget(Clear, dd_area);
                f.render_stateful_widget(list, dd_area, &mut state);
            }
        }

        if let Some(picker) = &ut.host_picker {
            super::host_picker::draw(f, picker, area);
        }
    }

    fn draw_profiles(&self, f: &mut Frame, area: Rect) {
        let pt = &self.profiles_tab;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(11)])
            .split(area);

        let header = Row::new(vec![
            ratatui::widgets::Cell::from(Span::styled(
                "Username",
                Style::default().fg(TITLE).add_modifier(ratatui::style::Modifier::BOLD),
            )),
            ratatui::widgets::Cell::from(Span::styled(
                "SSH Key (truncated)",
                Style::default().fg(TITLE).add_modifier(ratatui::style::Modifier::BOLD),
            )),
        ])
        .style(Style::default().bg(BG2));

        let rows: Vec<Row> = self
            .cfg
            .profiles
            .iter()
            .map(|p| {
                let trunc = if p.key.len() > 60 {
                    format!("{}...", &p.key[..60])
                } else {
                    p.key.clone()
                };
                Row::new(vec![
                    ratatui::widgets::Cell::from(p.name.clone()),
                    ratatui::widgets::Cell::from(trunc),
                ])
            })
            .collect();

        let table = Table::new(rows, [Constraint::Length(20), Constraint::Min(0)])
            .header(header)
            .block(theme_block(" Profiles "))
            .style(Style::default().fg(FG).bg(BG));
        f.render_widget(table, chunks[0]);

        let form_block = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::TOP)
            .border_style(Style::default().fg(BORDER))
            .style(Style::default().bg(BG));
        let form_inner = form_block.inner(chunks[1]);
        f.render_widget(form_block, chunks[1]);

        let form_rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(form_inner);

        let label_w: u16 = 14;
        let fw = form_rows[0].width.saturating_sub(label_w) as usize;
        let del_fw = form_rows[6].width.saturating_sub(label_w + 2 + 10) as usize;

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Name:         ", lbl()),
                input_span(&pt.name, pt.field == ProfilesField::Name, false, fw),
            ])),
            form_rows[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Key:          ", lbl()),
                input_span(&pt.key, pt.field == ProfilesField::Key, false, fw),
            ])),
            form_rows[2],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![btn_span("Add", pt.field == ProfilesField::BtnAdd)])),
            form_rows[4],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Delete name:  ", lbl()),
                input_span(&pt.del_name, pt.field == ProfilesField::DelName, false, del_fw),
                Span::raw("  "),
                btn_span("Delete", pt.field == ProfilesField::BtnDel),
            ])),
            form_rows[6],
        );
    }

    fn draw_settings(&self, f: &mut Frame, area: Rect) {
        let st = &self.settings_tab;
        let block = theme_block(" Settings ");
        let inner = block.inner(area);
        f.render_widget(block, area);

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
                Span::styled("SSH User:     ", lbl()),
                input_span(&st.ssh_user, st.field == SettingsField::SshUser, false, fw),
            ])),
            rows[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("SSH Key Path: ", lbl()),
                input_span(&st.ssh_key_path, st.field == SettingsField::SshKeyPath, false, fw),
            ])),
            rows[2],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("SSH Password: ", lbl()),
                input_span(&st.ssh_password, st.field == SettingsField::SshPassword, true, fw),
            ])),
            rows[4],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Port:         ", lbl()),
                input_span(&st.port, st.field == SettingsField::Port, false, fw),
            ])),
            rows[6],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![btn_span("Save", st.field == SettingsField::BtnSave)])),
            rows[8],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Tab/\u{2191}\u{2193} navigate  Enter to save  Esc back",
                lbl(),
            ))),
            rows[10],
        );
    }
}

fn filtered_profiles<'a>(profiles: &'a [config::Profile], query: &str) -> Vec<&'a str> {
    let q = query.to_lowercase();
    profiles
        .iter()
        .map(|p| p.name.as_str())
        .filter(|name| q.is_empty() || name.to_lowercase().contains(&q))
        .collect()
}
