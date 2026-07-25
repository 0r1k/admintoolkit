//! SSH Server Manager (easyssh) — browse/add/edit/delete hosts straight
//! from `~/.ssh/config` (the same file the real `ssh` binary reads), tag
//! and group them, connect with one keypress, pin favorites, ping, and
//! manage background port forwarding. See `crate::easyssh_mgr` for the
//! config-file parser and CRUD layer.

use std::{sync::mpsc, thread, time::Duration};

use arboard::Clipboard;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};

use crate::easyssh_mgr::{
    config::{self, Server},
    launcher,
};

use super::file_picker::FilePicker;
use super::mouse;
use super::widgets::*;
use super::PendingAction;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Servers,
    Tags,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Search,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TagsView {
    List,
    Filtered,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortMode {
    AliasAsc,
    AliasDesc,
    LastSeenAsc,
    LastSeenDesc,
}

impl SortMode {
    fn label(&self) -> &'static str {
        match self {
            SortMode::AliasAsc => "Alias \u{2191}",
            SortMode::AliasDesc => "Alias \u{2193}",
            SortMode::LastSeenAsc => "Last SSH \u{2191}",
            SortMode::LastSeenDesc => "Last SSH \u{2193}",
        }
    }
    fn toggle_field(&self) -> Self {
        match self {
            SortMode::AliasAsc => SortMode::LastSeenAsc,
            SortMode::LastSeenAsc => SortMode::AliasAsc,
            SortMode::AliasDesc => SortMode::LastSeenDesc,
            SortMode::LastSeenDesc => SortMode::AliasDesc,
        }
    }
    fn reverse(&self) -> Self {
        match self {
            SortMode::AliasAsc => SortMode::AliasDesc,
            SortMode::AliasDesc => SortMode::AliasAsc,
            SortMode::LastSeenAsc => SortMode::LastSeenDesc,
            SortMode::LastSeenDesc => SortMode::LastSeenAsc,
        }
    }
}

/// Pinned servers always sort first (newest pin first); everything else
/// follows `mode`. Entries with no `last_seen` sort to the bottom under
/// either last-seen direction.
fn compare_servers(a: &Server, b: &Server, mode: SortMode) -> std::cmp::Ordering {
    let pa = a.pinned_at.is_some();
    let pb = b.pinned_at.is_some();
    if pa != pb {
        return pb.cmp(&pa);
    }
    if pa && pb {
        return b.pinned_at.cmp(&a.pinned_at).then_with(|| a.alias.to_lowercase().cmp(&b.alias.to_lowercase()));
    }
    match mode {
        SortMode::AliasAsc => a.alias.to_lowercase().cmp(&b.alias.to_lowercase()),
        SortMode::AliasDesc => b.alias.to_lowercase().cmp(&a.alias.to_lowercase()),
        SortMode::LastSeenAsc | SortMode::LastSeenDesc => {
            let za = a.last_seen.is_none();
            let zb = b.last_seen.is_none();
            if za != zb {
                return za.cmp(&zb);
            }
            if !za && !zb && a.last_seen != b.last_seen {
                return if mode == SortMode::LastSeenDesc { b.last_seen.cmp(&a.last_seen) } else { a.last_seen.cmp(&b.last_seen) };
            }
            a.alias.to_lowercase().cmp(&b.alias.to_lowercase())
        }
    }
}

fn matches_query(s: &Server, q: &str) -> bool {
    s.alias.to_lowercase().contains(q) || s.effective_host().to_lowercase().contains(q) || s.user.to_lowercase().contains(q) || s.tags.iter().any(|t| t.to_lowercase().contains(q))
}

fn parse_csv(raw: &str) -> Vec<String> {
    raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn extra_to_text(extra: &[(String, String)]) -> String {
    extra.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("; ")
}

fn parse_advanced(raw: &str) -> Vec<(String, String)> {
    raw.split(';')
        .filter_map(|part| {
            let part = part.trim();
            let (k, v) = part.split_once(':')?;
            let (k, v) = (k.trim(), v.trim());
            if k.is_empty() || v.is_empty() {
                None
            } else {
                Some((k.to_string(), v.to_string()))
            }
        })
        .collect()
}

// ── Add / Edit Server form ──────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditField {
    Alias,
    Host,
    User,
    Port,
    SshPassword,
    IdentityFiles,
    ProxyJump,
    LocalForward,
    ForwardAgent,
    Tags,
    Advanced,
    BtnSave,
    BtnCancel,
}

struct ServerForm {
    /// Original server (Add mode: `Server::default()`, empty alias). Fields
    /// this form doesn't expose (RemoteForward, DynamicForward,
    /// StrictHostKeyChecking, ConnectTimeout, ...) pass through untouched
    /// from here.
    base: Server,
    alias: Input,
    host: Input,
    user: Input,
    port: Input,
    /// Optional, stored encrypted in `easyssh.json` (never in
    /// `~/.ssh/config`, which has no such field) — see `Server::ssh_password`.
    /// Left blank on edit keeps whatever password is already saved rather
    /// than clearing it, matching the DB managers' convention.
    ssh_password: Input,
    identity_files: Input,
    proxy_jump: Input,
    local_forward: Input,
    /// 0 = unset, 1 = yes, 2 = no.
    forward_agent: u8,
    tags: Input,
    advanced: Input,
    field: EditField,
    key_picker: Option<FilePicker>,
}

impl ServerForm {
    fn new_add() -> Self {
        Self {
            base: Server::default(),
            alias: Input::default(),
            host: Input::default(),
            user: Input::default(),
            port: Input::new("22"),
            ssh_password: Input::default(),
            identity_files: Input::default(),
            proxy_jump: Input::default(),
            local_forward: Input::default(),
            forward_agent: 0,
            tags: Input::default(),
            advanced: Input::default(),
            field: EditField::Alias,
            key_picker: None,
        }
    }

    fn new_edit(server: &Server) -> Self {
        let port = if server.port == 0 { String::new() } else { server.port.to_string() };
        Self {
            base: server.clone(),
            alias: Input::new(&server.alias),
            host: Input::new(&server.host),
            user: Input::new(&server.user),
            port: Input::new(&port),
            ssh_password: Input::default(),
            identity_files: Input::new(&server.identity_files.join(", ")),
            proxy_jump: Input::new(&server.proxy_jump),
            local_forward: Input::new(&server.local_forward.join(", ")),
            forward_agent: match server.forward_agent.to_lowercase().as_str() {
                "yes" => 1,
                "no" => 2,
                _ => 0,
            },
            tags: Input::new(&server.tags.join(", ")),
            advanced: Input::new(&extra_to_text(&server.extra)),
            field: EditField::Alias,
            key_picker: None,
        }
    }

    fn is_add(&self) -> bool {
        self.base.alias.is_empty()
    }

    fn next_field(&mut self) {
        self.field = match self.field {
            EditField::Alias => EditField::Host,
            EditField::Host => EditField::User,
            EditField::User => EditField::Port,
            EditField::Port => EditField::SshPassword,
            EditField::SshPassword => EditField::IdentityFiles,
            EditField::IdentityFiles => EditField::ProxyJump,
            EditField::ProxyJump => EditField::LocalForward,
            EditField::LocalForward => EditField::ForwardAgent,
            EditField::ForwardAgent => EditField::Tags,
            EditField::Tags => EditField::Advanced,
            EditField::Advanced => EditField::BtnSave,
            EditField::BtnSave => EditField::BtnCancel,
            EditField::BtnCancel => EditField::Alias,
        };
    }
    fn prev_field(&mut self) {
        self.field = match self.field {
            EditField::Alias => EditField::BtnCancel,
            EditField::Host => EditField::Alias,
            EditField::User => EditField::Host,
            EditField::Port => EditField::User,
            EditField::SshPassword => EditField::Port,
            EditField::IdentityFiles => EditField::SshPassword,
            EditField::ProxyJump => EditField::IdentityFiles,
            EditField::LocalForward => EditField::ProxyJump,
            EditField::ForwardAgent => EditField::LocalForward,
            EditField::Tags => EditField::ForwardAgent,
            EditField::Advanced => EditField::Tags,
            EditField::BtnSave => EditField::Advanced,
            EditField::BtnCancel => EditField::BtnSave,
        };
    }

    fn build_server(&self) -> Server {
        let mut s = self.base.clone();
        s.alias = self.alias.value().trim().to_string();
        s.patterns = vec![s.alias.clone()];
        s.host = self.host.value().trim().to_string();
        s.user = self.user.value().trim().to_string();
        s.port = self.port.value().trim().parse().unwrap_or(0);
        if !self.ssh_password.value().is_empty() {
            s.ssh_password = self.ssh_password.value().to_string();
        }
        s.identity_files = parse_csv(self.identity_files.value());
        s.proxy_jump = self.proxy_jump.value().trim().to_string();
        s.local_forward = parse_csv(self.local_forward.value());
        s.forward_agent = match self.forward_agent {
            1 => "yes".to_string(),
            2 => "no".to_string(),
            _ => String::new(),
        };
        s.tags = parse_csv(self.tags.value());
        s.extra = parse_advanced(self.advanced.value());
        s
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditTagsField {
    Tags,
    BtnSave,
    BtnCancel,
}

struct EditTagsModal {
    alias: String,
    tags: Input,
    field: EditTagsField,
}

impl EditTagsModal {
    fn new(server: &Server) -> Self {
        Self { alias: server.alias.clone(), tags: Input::new(&server.tags.join(", ")), field: EditTagsField::Tags }
    }
    fn next_field(&mut self) {
        self.field = match self.field {
            EditTagsField::Tags => EditTagsField::BtnSave,
            EditTagsField::BtnSave => EditTagsField::BtnCancel,
            EditTagsField::BtnCancel => EditTagsField::Tags,
        };
    }
    fn prev_field(&mut self) {
        self.field = match self.field {
            EditTagsField::Tags => EditTagsField::BtnCancel,
            EditTagsField::BtnSave => EditTagsField::Tags,
            EditTagsField::BtnCancel => EditTagsField::BtnSave,
        };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfirmField {
    BtnDelete,
    BtnCancel,
}

struct ConfirmDeleteModal {
    alias: String,
    field: ConfirmField,
}

impl ConfirmDeleteModal {
    fn new(alias: String) -> Self {
        Self { alias, field: ConfirmField::BtnCancel }
    }
}

enum Modal {
    Add(ServerForm),
    Edit(ServerForm),
    EditTags(EditTagsModal),
    ConfirmDelete(ConfirmDeleteModal),
}

enum Msg {
    PingResult(String, Result<Duration, String>),
}

pub struct EasySshScreen {
    tab: Tab,
    servers: Vec<Server>,
    search: Input,
    focus: Focus,
    sort_mode: SortMode,
    selected_row: usize,
    tags_view: TagsView,
    tags_selected_row: usize,
    tag_filter: String,
    modal: Option<Modal>,
    history: Vec<(bool, String)>,
    /// Lines scrolled up from the newest entry in the History panel — see `widgets::draw_history`.
    history_scroll: u16,
    pending_action: Option<PendingAction>,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
}

impl EasySshScreen {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let mut screen = Self {
            tab: Tab::Servers,
            servers: Vec::new(),
            search: Input::default(),
            focus: Focus::List,
            sort_mode: SortMode::AliasAsc,
            selected_row: 0,
            tags_view: TagsView::List,
            tags_selected_row: 0,
            tag_filter: String::new(),
            modal: None,
            history: Vec::new(),
            history_scroll: 0,
            pending_action: None,
            tx,
            rx,
        };
        screen.refresh();
        screen
    }

    fn refresh(&mut self) {
        match config::list_servers("") {
            Ok(servers) => self.servers = servers,
            Err(e) => self.history.push((false, format!("Failed to load ~/.ssh/config: {e}"))),
        }
    }

    pub fn tick(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::PingResult(alias, Ok(dur)) => self.history.push((true, format!("Ping {alias}: UP ({dur:?})"))),
                Msg::PingResult(alias, Err(e)) => self.history.push((false, format!("Ping {alias}: DOWN ({e})"))),
            }
        }
    }

    pub fn take_pending_action(&mut self) -> Option<PendingAction> {
        self.pending_action.take()
    }

    pub fn on_interactive_done(&mut self, alias: &str, status: std::io::Result<std::process::ExitStatus>) {
        match status {
            Ok(s) if s.success() => self.history.push((true, format!("ssh {alias}: session ended"))),
            Ok(s) => self.history.push((false, format!("ssh {alias}: exited with {s}"))),
            Err(e) => self.history.push((false, format!("ssh {alias}: failed to launch ({e})"))),
        }
        let _ = config::record_ssh(alias);
        self.refresh();
    }

    // ── Row sources ──────────────────────────────────────────────────

    fn rows_for_servers_tab(&self) -> Vec<&Server> {
        let q = self.search.value().trim().to_lowercase();
        let mut rows: Vec<&Server> = self.servers.iter().filter(|s| q.is_empty() || matches_query(s, &q)).collect();
        rows.sort_by(|a, b| compare_servers(a, b, self.sort_mode));
        rows
    }

    fn rows_for_tag(&self, tag: &str) -> Vec<&Server> {
        let mut rows: Vec<&Server> =
            self.servers.iter().filter(|s| if tag.is_empty() { s.tags.is_empty() } else { s.tags.iter().any(|t| t == tag) }).collect();
        rows.sort_by(|a, b| compare_servers(a, b, self.sort_mode));
        rows
    }

    /// Whichever list is on screen right now — the thing row-action keys
    /// (Enter/e/d/p/t/c/g/f/x) operate on.
    fn current_rows(&self) -> Vec<&Server> {
        if self.tab == Tab::Tags && self.tags_view == TagsView::Filtered {
            self.rows_for_tag(&self.tag_filter)
        } else {
            self.rows_for_servers_tab()
        }
    }

    fn distinct_tags(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        let mut untagged = 0usize;
        for s in &self.servers {
            if s.tags.is_empty() {
                untagged += 1;
            }
            for t in &s.tags {
                *counts.entry(t.clone()).or_insert(0) += 1;
            }
        }
        let mut out: Vec<(String, usize)> = counts.into_iter().collect();
        if untagged > 0 {
            out.push(("(untagged)".to_string(), untagged));
        }
        out
    }

    // ── Key handling ─────────────────────────────────────────────────

    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
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

        if let Some(modal) = self.modal.take() {
            let (modal, close) = self.handle_modal_key(modal, key);
            if !close {
                self.modal = Some(modal);
            }
            return false;
        }

        match key.code {
            KeyCode::F(1) => {
                self.tab = Tab::Servers;
                return false;
            }
            KeyCode::F(2) => {
                self.tab = Tab::Tags;
                return false;
            }
            _ => {}
        }

        match self.tab {
            Tab::Servers => self.handle_servers_key(key),
            Tab::Tags => self.handle_tags_key(key),
        }
    }

    pub fn handle_mouse(&mut self, me: MouseEvent, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(area);

        if self.modal.is_some() {
            self.handle_modal_mouse(me, area);
            return;
        }

        if let Some((x, y)) = mouse::left_click(&me) {
            if let Some(i) = mouse::label_row_hit(x, y, chunks[0], &["F1 Servers", "F2 Tags"]) {
                self.tab = if i == 0 { Tab::Servers } else { Tab::Tags };
                return;
            }
        }

        match self.tab {
            Tab::Servers => self.handle_servers_mouse(me, chunks[1]),
            Tab::Tags => self.handle_tags_mouse(me, chunks[1]),
        }
    }

    fn handle_servers_mouse(&mut self, me: MouseEvent, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(6), Constraint::Length(2), Constraint::Length(6)])
            .split(area);

        if let Some(delta) = mouse::scroll_delta(&me) {
            let n = self.rows_for_servers_tab().len();
            if n > 0 && mouse::in_rect(chunks[1], me.column, me.row) {
                if delta < 0 && self.selected_row > 0 {
                    self.selected_row -= 1;
                } else if delta > 0 && self.selected_row + 1 < n {
                    self.selected_row += 1;
                }
            } else if mouse::in_rect(chunks[3], me.column, me.row) {
                self.history_scroll =
                    if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };

        let search_inner = mouse::block_inner(chunks[0]);
        if mouse::in_rect(search_inner, x, y) {
            self.focus = Focus::Search;
            return;
        }

        let rows_len = self.rows_for_servers_tab().len();
        if let Some(idx) = mouse::table_row_hit(x, y, chunks[1], 1, rows_len, self.selected_row) {
            self.focus = Focus::List;
            self.selected_row = idx;
            self.trigger_ssh_connect();
        }
    }

    fn handle_tags_mouse(&mut self, me: MouseEvent, area: Rect) {
        match self.tags_view {
            TagsView::List => {
                let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(6), Constraint::Length(2)]).split(area);
                let tags = self.distinct_tags();
                if let Some(delta) = mouse::scroll_delta(&me) {
                    if !tags.is_empty() && mouse::in_rect(chunks[0], me.column, me.row) {
                        if delta < 0 && self.tags_selected_row > 0 {
                            self.tags_selected_row -= 1;
                        } else if delta > 0 && self.tags_selected_row + 1 < tags.len() {
                            self.tags_selected_row += 1;
                        }
                    }
                    return;
                }
                let Some((x, y)) = mouse::left_click(&me) else { return };
                if let Some(idx) = mouse::table_row_hit(x, y, chunks[0], 0, tags.len(), self.tags_selected_row) {
                    if let Some((tag, _)) = tags.get(idx) {
                        self.tag_filter = if tag == "(untagged)" { String::new() } else { tag.clone() };
                        self.tags_view = TagsView::Filtered;
                        self.selected_row = 0;
                    }
                }
            }
            TagsView::Filtered => {
                let chunks =
                    Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(6), Constraint::Length(2), Constraint::Length(6)]).split(area);
                let rows_len = self.rows_for_tag(&self.tag_filter).len();
                if let Some(delta) = mouse::scroll_delta(&me) {
                    if rows_len > 0 && mouse::in_rect(chunks[0], me.column, me.row) {
                        if delta < 0 && self.selected_row > 0 {
                            self.selected_row -= 1;
                        } else if delta > 0 && self.selected_row + 1 < rows_len {
                            self.selected_row += 1;
                        }
                    } else if mouse::in_rect(chunks[2], me.column, me.row) {
                        self.history_scroll =
                            if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
                    }
                    return;
                }
                let Some((x, y)) = mouse::left_click(&me) else { return };
                if let Some(idx) = mouse::table_row_hit(x, y, chunks[0], 1, rows_len, self.selected_row) {
                    self.selected_row = idx;
                    self.trigger_ssh_connect();
                }
            }
        }
    }

    /// Only the small confirm-style modals (Save/Cancel, Delete/Cancel)
    /// get click support — the big Add/Edit Server form stays
    /// keyboard-only for now, its 12+ fields aren't worth the hit-test
    /// bookkeeping in this pass.
    fn handle_modal_mouse(&mut self, me: MouseEvent, area: Rect) {
        let Some((x, y)) = mouse::left_click(&me) else { return };
        let Some(modal) = self.modal.take() else { return };
        match modal {
            Modal::EditTags(mut m) => {
                let width = 60u16.min(area.width.saturating_sub(4));
                let height = 9u16.min(area.height.saturating_sub(2));
                let modal_area = centered_rect(width, height, area);
                let inner = mouse::block_inner(modal_area);
                let rows = Layout::default()
                    .direction(Direction::Vertical)
                    .margin(1)
                    .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)])
                    .split(inner);
                let mut close = false;
                if let Some(i) = mouse::button_row_hit(x, y, rows[3], &["Save", "Cancel"]) {
                    if i == 0 {
                        let tags = parse_csv(m.tags.value());
                        match config::set_tags(&m.alias, tags) {
                            Ok(()) => {
                                self.history.push((true, format!("Tags updated for {}", m.alias)));
                                self.refresh();
                            }
                            Err(e) => self.history.push((false, format!("Failed to update tags: {e}"))),
                        }
                    }
                    close = true;
                } else if mouse::in_rect(rows[0], x, y) {
                    m.field = EditTagsField::Tags;
                }
                if !close {
                    self.modal = Some(Modal::EditTags(m));
                }
            }
            Modal::ConfirmDelete(m) => {
                let width = 60u16.min(area.width.saturating_sub(4));
                let height = 7u16.min(area.height.saturating_sub(2));
                let modal_area = centered_rect(width, height, area);
                let inner = mouse::block_inner(modal_area);
                let rows = Layout::default()
                    .direction(Direction::Vertical)
                    .margin(1)
                    .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)])
                    .split(inner);
                if let Some(i) = mouse::button_row_hit(x, y, rows[2], &["Delete", "Cancel"]) {
                    if i == 0 {
                        match config::delete_server(&m.alias) {
                            Ok(()) => {
                                self.history.push((true, format!("Server {} deleted", m.alias)));
                                self.refresh();
                            }
                            Err(e) => self.history.push((false, format!("Failed to delete {}: {e}", m.alias))),
                        }
                    }
                } else {
                    self.modal = Some(Modal::ConfirmDelete(m));
                }
            }
            other => self.modal = Some(other),
        }
    }

    fn handle_servers_key(&mut self, key: KeyEvent) -> bool {
        if self.focus == Focus::Search {
            match key.code {
                KeyCode::Esc => {
                    self.focus = Focus::List;
                    return false;
                }
                KeyCode::Enter => {
                    self.focus = Focus::List;
                    self.selected_row = 0;
                    return false;
                }
                KeyCode::Up | KeyCode::Down => {
                    self.focus = Focus::List;
                    self.selected_row = 0;
                    return false;
                }
                KeyCode::Char(c) => {
                    self.search.insert(c);
                    self.selected_row = 0;
                    return false;
                }
                KeyCode::Backspace => {
                    self.search.backspace();
                    self.selected_row = 0;
                    return false;
                }
                KeyCode::Delete => {
                    self.search.delete();
                    return false;
                }
                KeyCode::Left => {
                    self.search.left();
                    return false;
                }
                KeyCode::Right => {
                    self.search.right();
                    return false;
                }
                KeyCode::Home => {
                    self.search.home();
                    return false;
                }
                KeyCode::End => {
                    self.search.end_of_line();
                    return false;
                }
                _ => return false,
            }
        }

        let rows_len = self.rows_for_servers_tab().len();
        match key.code {
            KeyCode::Esc => return true,
            KeyCode::Char('/') => self.focus = Focus::Search,
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_row > 0 {
                    self.selected_row -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected_row + 1 < rows_len {
                    self.selected_row += 1;
                }
            }
            KeyCode::Enter => self.trigger_ssh_connect(),
            KeyCode::Char('a') => self.modal = Some(Modal::Add(ServerForm::new_add())),
            KeyCode::Char('e') => self.trigger_edit(),
            KeyCode::Char('d') => self.trigger_delete_confirm(),
            KeyCode::Char('p') => self.trigger_pin_toggle(),
            KeyCode::Char('t') => self.trigger_edit_tags(),
            KeyCode::Char('c') => self.trigger_copy_command(),
            KeyCode::Char('g') => self.trigger_ping(),
            KeyCode::Char('r') => {
                self.refresh();
                self.history.push((true, "Refreshed".to_string()));
            }
            KeyCode::Char('f') => self.trigger_start_forward(),
            KeyCode::Char('x') => self.trigger_stop_forward(),
            KeyCode::Char('s') => self.sort_mode = self.sort_mode.toggle_field(),
            KeyCode::Char('S') => self.sort_mode = self.sort_mode.reverse(),
            _ => {}
        }
        false
    }

    fn handle_tags_key(&mut self, key: KeyEvent) -> bool {
        match self.tags_view {
            TagsView::List => {
                let tags = self.distinct_tags();
                match key.code {
                    KeyCode::Esc => return true,
                    KeyCode::Up | KeyCode::Char('k') => {
                        if self.tags_selected_row > 0 {
                            self.tags_selected_row -= 1;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if self.tags_selected_row + 1 < tags.len() {
                            self.tags_selected_row += 1;
                        }
                    }
                    KeyCode::Enter => {
                        if let Some((tag, _)) = tags.get(self.tags_selected_row) {
                            self.tag_filter = if tag == "(untagged)" { String::new() } else { tag.clone() };
                            self.tags_view = TagsView::Filtered;
                            self.selected_row = 0;
                        }
                    }
                    _ => {}
                }
            }
            TagsView::Filtered => {
                let rows_len = self.rows_for_tag(&self.tag_filter).len();
                match key.code {
                    KeyCode::Esc => self.tags_view = TagsView::List,
                    KeyCode::Up | KeyCode::Char('k') => {
                        if self.selected_row > 0 {
                            self.selected_row -= 1;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if self.selected_row + 1 < rows_len {
                            self.selected_row += 1;
                        }
                    }
                    KeyCode::Enter => self.trigger_ssh_connect(),
                    KeyCode::Char('e') => self.trigger_edit(),
                    KeyCode::Char('d') => self.trigger_delete_confirm(),
                    KeyCode::Char('p') => self.trigger_pin_toggle(),
                    KeyCode::Char('t') => self.trigger_edit_tags(),
                    KeyCode::Char('c') => self.trigger_copy_command(),
                    KeyCode::Char('g') => self.trigger_ping(),
                    KeyCode::Char('f') => self.trigger_start_forward(),
                    KeyCode::Char('x') => self.trigger_stop_forward(),
                    _ => {}
                }
            }
        }
        false
    }

    // ── Row-action triggers (shared by Servers tab & a drilled-into tag) ─

    fn trigger_ssh_connect(&mut self) {
        let rows = self.current_rows();
        let Some(alias) = rows.get(self.selected_row).map(|s| s.alias.clone()) else { return };
        self.pending_action = Some(PendingAction::RunInteractive { program: "ssh".to_string(), args: vec![alias.clone()], alias });
    }

    fn trigger_edit(&mut self) {
        let rows = self.current_rows();
        if let Some(server) = rows.get(self.selected_row).map(|s| (*s).clone()) {
            self.modal = Some(Modal::Edit(ServerForm::new_edit(&server)));
        }
    }

    fn trigger_delete_confirm(&mut self) {
        let rows = self.current_rows();
        if let Some(alias) = rows.get(self.selected_row).map(|s| s.alias.clone()) {
            self.modal = Some(Modal::ConfirmDelete(ConfirmDeleteModal::new(alias)));
        }
    }

    fn trigger_pin_toggle(&mut self) {
        let rows = self.current_rows();
        let Some((alias, pinned)) = rows.get(self.selected_row).map(|s| (s.alias.clone(), s.pinned_at.is_some())) else { return };
        match config::set_pinned(&alias, !pinned) {
            Ok(()) => {
                self.history.push((true, format!("{} {alias}", if pinned { "Unpinned" } else { "Pinned" })));
                self.refresh();
            }
            Err(e) => self.history.push((false, format!("Failed to pin/unpin {alias}: {e}"))),
        }
    }

    fn trigger_edit_tags(&mut self) {
        let rows = self.current_rows();
        if let Some(server) = rows.get(self.selected_row).map(|s| (*s).clone()) {
            self.modal = Some(Modal::EditTags(EditTagsModal::new(&server)));
        }
    }

    fn trigger_copy_command(&mut self) {
        let rows = self.current_rows();
        let Some(server) = rows.get(self.selected_row).map(|s| (*s).clone()) else { return };
        let cmd = launcher::build_ssh_command(&server);
        match Clipboard::new().and_then(|mut c| c.set_text(cmd.clone())) {
            Ok(()) => self.history.push((true, format!("Copied: {cmd}"))),
            Err(_) => self.history.push((false, "Failed to copy to clipboard".to_string())),
        }
    }

    fn trigger_ping(&mut self) {
        let rows = self.current_rows();
        let Some(server) = rows.get(self.selected_row).map(|s| (*s).clone()) else { return };
        let alias = server.alias.clone();
        self.history.push((true, format!("Pinging {alias}\u{2026}")));
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = launcher::ping(&server);
            let _ = tx.send(Msg::PingResult(alias, result));
        });
    }

    fn trigger_start_forward(&mut self) {
        let rows = self.current_rows();
        let Some(server) = rows.get(self.selected_row).map(|s| (*s).clone()) else { return };
        if server.local_forward.is_empty() && server.remote_forward.is_empty() && server.dynamic_forward.is_empty() {
            self.history
                .push((false, format!("{} has no LocalForward/RemoteForward/DynamicForward set (edit the server to add one)", server.alias)));
            return;
        }
        match launcher::start_forward(&server.alias) {
            Ok(pid) => self.history.push((true, format!("Port forwarding started for {} (pid {pid})", server.alias))),
            Err(e) => self.history.push((false, format!("Failed to start forwarding for {}: {e}", server.alias))),
        }
    }

    fn trigger_stop_forward(&mut self) {
        let rows = self.current_rows();
        let Some(alias) = rows.get(self.selected_row).map(|s| s.alias.clone()) else { return };
        match launcher::stop_forwarding(&alias) {
            Ok(()) => self.history.push((true, format!("Stopped forwarding for {alias}"))),
            Err(e) => self.history.push((false, format!("Failed to stop forwarding for {alias}: {e}"))),
        }
    }

    // ── Modal key handling ───────────────────────────────────────────

    fn handle_modal_key(&mut self, modal: Modal, key: KeyEvent) -> (Modal, bool) {
        match modal {
            Modal::Add(mut form) => {
                let close = self.handle_server_form_key(&mut form, key);
                (Modal::Add(form), close)
            }
            Modal::Edit(mut form) => {
                let close = self.handle_server_form_key(&mut form, key);
                (Modal::Edit(form), close)
            }
            Modal::EditTags(mut m) => {
                let close = self.handle_edit_tags_key(&mut m, key);
                (Modal::EditTags(m), close)
            }
            Modal::ConfirmDelete(mut m) => {
                let close = self.handle_confirm_delete_key(&mut m, key);
                (Modal::ConfirmDelete(m), close)
            }
        }
    }

    fn handle_server_form_key(&mut self, form: &mut ServerForm, key: KeyEvent) -> bool {
        if form.key_picker.is_some() {
            match key.code {
                KeyCode::Esc => form.key_picker = None,
                KeyCode::Up => {
                    if let Some(p) = form.key_picker.as_mut() {
                        p.up();
                    }
                }
                KeyCode::Down => {
                    if let Some(p) = form.key_picker.as_mut() {
                        p.down();
                    }
                }
                KeyCode::Enter => {
                    let picked = form.key_picker.as_mut().and_then(|p| p.activate());
                    if let Some(path) = picked {
                        let p = path.to_string_lossy().to_string();
                        form.identity_files = if form.identity_files.value().trim().is_empty() {
                            Input::new(&p)
                        } else {
                            Input::new(&format!("{}, {p}", form.identity_files.value()))
                        };
                        form.key_picker = None;
                    }
                }
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Esc => return true,
            KeyCode::Tab => form.next_field(),
            KeyCode::BackTab => form.prev_field(),
            KeyCode::Up => form.prev_field(),
            KeyCode::Down => form.next_field(),
            KeyCode::Left if form.field == EditField::ForwardAgent => form.forward_agent = (form.forward_agent + 2) % 3,
            KeyCode::Right if form.field == EditField::ForwardAgent => form.forward_agent = (form.forward_agent + 1) % 3,
            KeyCode::Enter => match form.field {
                EditField::IdentityFiles => form.key_picker = Some(FilePicker::new(form.identity_files.value())),
                EditField::ForwardAgent => form.forward_agent = (form.forward_agent + 1) % 3,
                EditField::BtnSave => {
                    let server = form.build_server();
                    let result = if form.is_add() { config::add_server(&server) } else { config::update_server(&form.base.alias, &server) };
                    match result {
                        Ok(()) => {
                            self.history.push((true, format!("Server {} saved", server.alias)));
                            self.refresh();
                            return true;
                        }
                        Err(e) => self.history.push((false, format!("Save failed: {e}"))),
                    }
                }
                EditField::BtnCancel => return true,
                _ => form.next_field(),
            },
            KeyCode::Char(c) => match form.field {
                EditField::Alias => form.alias.insert(c),
                EditField::Host => form.host.insert(c),
                EditField::User => form.user.insert(c),
                EditField::Port => form.port.insert(c),
                EditField::SshPassword => form.ssh_password.insert(c),
                EditField::IdentityFiles => form.identity_files.insert(c),
                EditField::ProxyJump => form.proxy_jump.insert(c),
                EditField::LocalForward => form.local_forward.insert(c),
                EditField::Tags => form.tags.insert(c),
                EditField::Advanced => form.advanced.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match form.field {
                EditField::Alias => form.alias.backspace(),
                EditField::Host => form.host.backspace(),
                EditField::User => form.user.backspace(),
                EditField::Port => form.port.backspace(),
                EditField::SshPassword => form.ssh_password.backspace(),
                EditField::IdentityFiles => form.identity_files.backspace(),
                EditField::ProxyJump => form.proxy_jump.backspace(),
                EditField::LocalForward => form.local_forward.backspace(),
                EditField::Tags => form.tags.backspace(),
                EditField::Advanced => form.advanced.backspace(),
                _ => {}
            },
            KeyCode::Delete => match form.field {
                EditField::Alias => form.alias.delete(),
                EditField::Host => form.host.delete(),
                EditField::User => form.user.delete(),
                EditField::Port => form.port.delete(),
                EditField::SshPassword => form.ssh_password.delete(),
                EditField::IdentityFiles => form.identity_files.delete(),
                EditField::ProxyJump => form.proxy_jump.delete(),
                EditField::LocalForward => form.local_forward.delete(),
                EditField::Tags => form.tags.delete(),
                EditField::Advanced => form.advanced.delete(),
                _ => {}
            },
            KeyCode::Left => match form.field {
                EditField::Alias => form.alias.left(),
                EditField::Host => form.host.left(),
                EditField::User => form.user.left(),
                EditField::Port => form.port.left(),
                EditField::SshPassword => form.ssh_password.left(),
                EditField::IdentityFiles => form.identity_files.left(),
                EditField::ProxyJump => form.proxy_jump.left(),
                EditField::LocalForward => form.local_forward.left(),
                EditField::Tags => form.tags.left(),
                EditField::Advanced => form.advanced.left(),
                _ => {}
            },
            KeyCode::Right => match form.field {
                EditField::Alias => form.alias.right(),
                EditField::Host => form.host.right(),
                EditField::User => form.user.right(),
                EditField::Port => form.port.right(),
                EditField::SshPassword => form.ssh_password.right(),
                EditField::IdentityFiles => form.identity_files.right(),
                EditField::ProxyJump => form.proxy_jump.right(),
                EditField::LocalForward => form.local_forward.right(),
                EditField::Tags => form.tags.right(),
                EditField::Advanced => form.advanced.right(),
                _ => {}
            },
            KeyCode::Home => match form.field {
                EditField::Alias => form.alias.home(),
                EditField::Host => form.host.home(),
                EditField::User => form.user.home(),
                EditField::Port => form.port.home(),
                EditField::SshPassword => form.ssh_password.home(),
                EditField::IdentityFiles => form.identity_files.home(),
                EditField::ProxyJump => form.proxy_jump.home(),
                EditField::LocalForward => form.local_forward.home(),
                EditField::Tags => form.tags.home(),
                EditField::Advanced => form.advanced.home(),
                _ => {}
            },
            KeyCode::End => match form.field {
                EditField::Alias => form.alias.end_of_line(),
                EditField::Host => form.host.end_of_line(),
                EditField::User => form.user.end_of_line(),
                EditField::Port => form.port.end_of_line(),
                EditField::SshPassword => form.ssh_password.end_of_line(),
                EditField::IdentityFiles => form.identity_files.end_of_line(),
                EditField::ProxyJump => form.proxy_jump.end_of_line(),
                EditField::LocalForward => form.local_forward.end_of_line(),
                EditField::Tags => form.tags.end_of_line(),
                EditField::Advanced => form.advanced.end_of_line(),
                _ => {}
            },
            _ => {}
        }
        false
    }

    fn handle_edit_tags_key(&mut self, m: &mut EditTagsModal, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => return true,
            KeyCode::Tab => m.next_field(),
            KeyCode::BackTab => m.prev_field(),
            KeyCode::Up => m.prev_field(),
            KeyCode::Down => m.next_field(),
            KeyCode::Enter => match m.field {
                EditTagsField::BtnSave => {
                    let tags = parse_csv(m.tags.value());
                    match config::set_tags(&m.alias, tags) {
                        Ok(()) => {
                            self.history.push((true, format!("Tags updated for {}", m.alias)));
                            self.refresh();
                        }
                        Err(e) => self.history.push((false, format!("Failed to update tags: {e}"))),
                    }
                    return true;
                }
                EditTagsField::BtnCancel => return true,
                _ => m.next_field(),
            },
            KeyCode::Char(c) if m.field == EditTagsField::Tags => m.tags.insert(c),
            KeyCode::Backspace if m.field == EditTagsField::Tags => m.tags.backspace(),
            KeyCode::Delete if m.field == EditTagsField::Tags => m.tags.delete(),
            KeyCode::Left if m.field == EditTagsField::Tags => m.tags.left(),
            KeyCode::Right if m.field == EditTagsField::Tags => m.tags.right(),
            KeyCode::Home if m.field == EditTagsField::Tags => m.tags.home(),
            KeyCode::End if m.field == EditTagsField::Tags => m.tags.end_of_line(),
            _ => {}
        }
        false
    }

    fn handle_confirm_delete_key(&mut self, m: &mut ConfirmDeleteModal, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => return true,
            KeyCode::Tab | KeyCode::Down | KeyCode::Right | KeyCode::BackTab | KeyCode::Up | KeyCode::Left => {
                m.field = match m.field {
                    ConfirmField::BtnDelete => ConfirmField::BtnCancel,
                    ConfirmField::BtnCancel => ConfirmField::BtnDelete,
                }
            }
            KeyCode::Enter => match m.field {
                ConfirmField::BtnDelete => {
                    match config::delete_server(&m.alias) {
                        Ok(()) => {
                            self.history.push((true, format!("Server {} deleted", m.alias)));
                            self.refresh();
                        }
                        Err(e) => self.history.push((false, format!("Failed to delete {}: {e}", m.alias))),
                    }
                    return true;
                }
                ConfirmField::BtnCancel => return true,
            },
            _ => {}
        }
        false
    }

    // ── Drawing ───────────────────────────────────────────────────────

    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(area);

        let tab_bar = Line::from(vec![
            tab_span("F1 Servers", self.tab == Tab::Servers),
            Span::styled("  ", Style::default().bg(BG)),
            tab_span("F2 Tags", self.tab == Tab::Tags),
            Span::styled("  ", Style::default().bg(BG)),
            Span::styled("Esc back  Ctrl+C quit", Style::default().fg(FG2).bg(BG)),
        ]);
        f.render_widget(Paragraph::new(tab_bar).style(Style::default().bg(BG)), chunks[0]);

        match self.tab {
            Tab::Servers => self.draw_servers(f, chunks[1]),
            Tab::Tags => self.draw_tags(f, chunks[1]),
        }

        if let Some(modal) = &self.modal {
            self.draw_modal_overlay(f, modal, area);
        }
    }

    fn draw_servers(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(6), Constraint::Length(2), Constraint::Length(6)])
            .split(area);

        let search_block = theme_block(" Search ");
        let search_inner = search_block.inner(chunks[0]);
        f.render_widget(search_block, chunks[0]);
        let search_rows = Layout::default().direction(Direction::Vertical).margin(1).constraints([Constraint::Length(1)]).split(search_inner);
        let fw = search_rows[0].width.saturating_sub(40) as usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Search (/): ", lbl()),
                input_span(&self.search, self.focus == Focus::Search, false, fw),
                Span::styled(format!("   Sort: {}", self.sort_mode.label()), lbl()),
            ])),
            search_rows[0],
        );

        let rows = self.rows_for_servers_tab();
        self.draw_server_table(f, &rows, chunks[1], &format!(" Servers ({}) ", rows.len()), self.focus == Focus::List);

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Enter connect  a add  e edit  d delete  p pin  t tags  c copy  g ping  f fwd start  x fwd stop  r refresh  s/S sort  / search  F2 tag groups",
                lbl(),
            )))
            .wrap(Wrap { trim: true }),
            chunks[2],
        );

        draw_history(f, &self.history, chunks[3], self.history_scroll);
    }

    fn draw_tags(&self, f: &mut Frame, area: Rect) {
        match self.tags_view {
            TagsView::List => self.draw_tags_list(f, area),
            TagsView::Filtered => self.draw_tags_filtered(f, area),
        }
    }

    fn draw_tags_list(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(6), Constraint::Length(2)]).split(area);
        let tags = self.distinct_tags();
        let items: Vec<ListItem> = tags
            .iter()
            .map(|(tag, count)| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("  {tag}"), Style::default().fg(FG)),
                    Span::styled(format!("  ({count})"), Style::default().fg(FG2)),
                ]))
            })
            .collect();
        let title = format!(" Tags ({}) \u{2014} group servers by tag, like folders ", tags.len());
        let list = List::new(items).block(theme_block(&title)).style(Style::default().bg(BG)).highlight_style(focused()).highlight_symbol(" \u{25B6} ");
        let mut lstate = ListState::default();
        if !tags.is_empty() {
            lstate.select(Some(self.tags_selected_row.min(tags.len() - 1)));
        }
        f.render_stateful_widget(list, chunks[0], &mut lstate);
        f.render_widget(Paragraph::new(Line::from(Span::styled("\u{2191}\u{2193} select  Enter open group  Esc back", lbl()))), chunks[1]);
    }

    fn draw_tags_filtered(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(6), Constraint::Length(2), Constraint::Length(6)]).split(area);
        let rows = self.rows_for_tag(&self.tag_filter);
        let label = if self.tag_filter.is_empty() { "(untagged)" } else { &self.tag_filter };
        self.draw_server_table(f, &rows, chunks[0], &format!(" Group: {label} ({}) ", rows.len()), true);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("Enter connect  e edit  d delete  p pin  t tags  c copy  g ping  f fwd start  x fwd stop  Esc back to tags", lbl())))
                .wrap(Wrap { trim: true }),
            chunks[1],
        );
        draw_history(f, &self.history, chunks[2], self.history_scroll);
    }

    fn draw_server_table(&self, f: &mut Frame, rows: &[&Server], area: Rect, title: &str, focused_table: bool) {
        let header = Row::new(vec![
            Cell::from(""),
            Cell::from(Span::styled("Alias", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Host", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("User", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Tags", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Last SSH", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Fwd", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
        ])
        .style(Style::default().bg(BG2));

        let table_rows: Vec<Row> = rows
            .iter()
            .map(|s| {
                let pin = if s.is_pinned() { "\u{2605}" } else { "" };
                let fwd = if launcher::is_forwarding(&s.alias) { "F" } else { "" };
                Row::new(vec![
                    Cell::from(Span::styled(pin, Style::default().fg(YELLOW))),
                    Cell::from(s.alias.clone()),
                    Cell::from(s.effective_host().to_string()),
                    Cell::from(s.user.clone()),
                    Cell::from(s.tags.join(", ")),
                    Cell::from(config::humanize_timestamp(&s.last_seen)),
                    Cell::from(Span::styled(fwd, Style::default().fg(GREEN))),
                ])
            })
            .collect();

        let table = Table::new(
            table_rows,
            [
                Constraint::Length(2),
                Constraint::Length(16),
                Constraint::Length(20),
                Constraint::Length(10),
                Constraint::Length(18),
                Constraint::Length(12),
                Constraint::Length(4),
            ],
        )
        .header(header)
        .block(theme_block(title))
        .row_highlight_style(if focused_table { focused() } else { normal() })
        .highlight_symbol(" \u{25B6} ")
        .style(Style::default().fg(FG).bg(BG));

        let mut tstate = TableState::default();
        if !rows.is_empty() {
            tstate.select(Some(self.selected_row.min(rows.len() - 1)));
        }
        f.render_stateful_widget(table, area, &mut tstate);
    }

    fn draw_modal_overlay(&self, f: &mut Frame, modal: &Modal, area: Rect) {
        match modal {
            Modal::Add(form) => self.draw_server_form(f, form, area),
            Modal::Edit(form) => self.draw_server_form(f, form, area),
            Modal::EditTags(m) => self.draw_edit_tags_modal(f, m, area),
            Modal::ConfirmDelete(m) => self.draw_confirm_delete_modal(f, m, area),
        }
    }

    fn draw_server_form(&self, f: &mut Frame, form: &ServerForm, area: Rect) {
        let width = 76u16.min(area.width.saturating_sub(4));
        let height = 32u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);
        f.render_widget(Clear, modal_area);
        let title = if form.is_add() { " Add Server " } else { " Edit Server " };
        let block = Block::default()
            .title(Span::styled(title, Style::default().fg(TITLE)))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(BG2));
        let inner = block.inner(modal_area);
        f.render_widget(block, modal_area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // 0 Alias
                Constraint::Length(1), // 1 spacer
                Constraint::Length(1), // 2 Host
                Constraint::Length(1), // 3 spacer
                Constraint::Length(1), // 4 User
                Constraint::Length(1), // 5 spacer
                Constraint::Length(1), // 6 Port
                Constraint::Length(1), // 7 spacer
                Constraint::Length(1), // 8 SSH Password
                Constraint::Length(1), // 9 spacer
                Constraint::Length(1), // 10 Identity Files
                Constraint::Length(1), // 11 hint
                Constraint::Length(1), // 12 spacer
                Constraint::Length(1), // 13 Proxy Jump
                Constraint::Length(1), // 14 spacer
                Constraint::Length(1), // 15 Local Forward
                Constraint::Length(1), // 16 hint
                Constraint::Length(1), // 17 spacer
                Constraint::Length(1), // 18 Forward Agent
                Constraint::Length(1), // 19 spacer
                Constraint::Length(1), // 20 Tags
                Constraint::Length(1), // 21 spacer
                Constraint::Length(1), // 22 Advanced
                Constraint::Length(1), // 23 hint
                Constraint::Length(1), // 24 spacer
                Constraint::Length(1), // 25 buttons
                Constraint::Length(1), // 26 spacer
                Constraint::Length(1), // 27 nav hint
                Constraint::Min(0),
            ])
            .split(inner);
        let fw = rows[0].width.saturating_sub(16) as usize;

        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Alias:          ", lbl()), input_span(&form.alias, form.field == EditField::Alias, false, fw)])),
            rows[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Host:           ", lbl()), input_span(&form.host, form.field == EditField::Host, false, fw)])),
            rows[2],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("User:           ", lbl()), input_span(&form.user, form.field == EditField::User, false, fw)])),
            rows[4],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Port:           ", lbl()), input_span(&form.port, form.field == EditField::Port, false, fw)])),
            rows[6],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("SSH Password:   ", lbl()),
                input_span(&form.ssh_password, form.field == EditField::SshPassword, true, fw),
            ])),
            rows[8],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Identity Files: ", lbl()),
                input_span(&form.identity_files, form.field == EditField::IdentityFiles, false, fw),
            ])),
            rows[10],
        );
        f.render_widget(Paragraph::new(Line::from(Span::styled("Enter opens a file picker; comma-separated for multiple", lbl()))), rows[11]);
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Proxy Jump:     ", lbl()), input_span(&form.proxy_jump, form.field == EditField::ProxyJump, false, fw)])),
            rows[13],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Local Forward:  ", lbl()),
                input_span(&form.local_forward, form.field == EditField::LocalForward, false, fw),
            ])),
            rows[15],
        );
        f.render_widget(Paragraph::new(Line::from(Span::styled("format port:host:hostport, comma-separated for multiple", lbl()))), rows[16]);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Forward Agent:  ", lbl()),
                btn_span(match form.forward_agent { 1 => "Yes", 2 => "No", _ => "(unset)" }, form.field == EditField::ForwardAgent),
                Span::styled("  (\u{2190}/\u{2192} to change)", lbl()),
            ])),
            rows[18],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Tags:           ", lbl()), input_span(&form.tags, form.field == EditField::Tags, false, fw)])),
            rows[20],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Advanced:       ", lbl()), input_span(&form.advanced, form.field == EditField::Advanced, false, fw)])),
            rows[22],
        );
        f.render_widget(Paragraph::new(Line::from(Span::styled("any other ssh_config directive: Key: Value; Key2: Value2", lbl()))), rows[23]);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                btn_span("Save", form.field == EditField::BtnSave),
                Span::raw("  "),
                btn_span("Cancel", form.field == EditField::BtnCancel),
            ])),
            rows[25],
        );
        f.render_widget(Paragraph::new(Line::from(Span::styled("Tab navigate  \u{2022}  Enter activate  \u{2022}  Esc cancel", lbl()))), rows[27]);

        if let Some(picker) = &form.key_picker {
            super::file_picker::draw(f, picker, area);
        }
    }

    fn draw_edit_tags_modal(&self, f: &mut Frame, m: &EditTagsModal, area: Rect) {
        let width = 60u16.min(area.width.saturating_sub(4));
        let height = 9u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);
        f.render_widget(Clear, modal_area);
        let block = Block::default()
            .title(Span::styled(format!(" Edit Tags: {} ", m.alias), Style::default().fg(TITLE)))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(BG2));
        let inner = block.inner(modal_area);
        f.render_widget(block, modal_area);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)])
            .split(inner);
        let fw = rows[0].width.saturating_sub(8) as usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Tags: ", lbl()), input_span(&m.tags, m.field == EditTagsField::Tags, false, fw)])),
            rows[0],
        );
        f.render_widget(Paragraph::new(Line::from(Span::styled("comma-separated, e.g. VPS, prod", lbl()))), rows[1]);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                btn_span("Save", m.field == EditTagsField::BtnSave),
                Span::raw("  "),
                btn_span("Cancel", m.field == EditTagsField::BtnCancel),
            ])),
            rows[3],
        );
    }

    fn draw_confirm_delete_modal(&self, f: &mut Frame, m: &ConfirmDeleteModal, area: Rect) {
        let width = 60u16.min(area.width.saturating_sub(4));
        let height = 7u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);
        f.render_widget(Clear, modal_area);
        let block = Block::default()
            .title(Span::styled(" Delete Server ", Style::default().fg(TITLE)))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(RED))
            .style(Style::default().bg(BG2));
        let inner = block.inner(modal_area);
        f.render_widget(block, modal_area);
        let rows = Layout::default().direction(Direction::Vertical).margin(1).constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]).split(inner);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(format!("Delete server '{}'? This removes its Host block from ~/.ssh/config.", m.alias), Style::default().fg(FG))))
                .wrap(Wrap { trim: true }),
            rows[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                btn_span("Delete", m.field == ConfirmField::BtnDelete),
                Span::raw("  "),
                btn_span("Cancel", m.field == ConfirmField::BtnCancel),
            ])),
            rows[2],
        );
    }
}

