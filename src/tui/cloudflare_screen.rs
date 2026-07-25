//! Cloudflare DNS Manager screen — manage multiple Cloudflare accounts
//! (API Token, encrypted at rest), search/filter DNS records across all of
//! them, and add/edit/delete individual records via modal dialogs.
//!
//! Structurally mirrors `godaddy_screen.rs` (same tab/modal/mouse-hit-test
//! shape), adapted for real Cloudflare differences: a single API Token
//! instead of a key+secret pair, "zone" instead of "domain" (Cloudflare's
//! own term), a "Proxied" toggle on the types that support it, and real
//! per-record IDs — so Update is a genuine `PUT` by ID instead of GoDaddy's
//! delete-then-recreate workaround.
//!
//! Records tab data model: `rows` holds whatever was last fetched (one
//! zone, one account's every zone, or — via "Fetch All Accounts" — every
//! zone of every account). Search / Account / Type act as a client-side
//! filter over `rows`, so a global cross-account search is just: Fetch All
//! Accounts once, then type into Search.

use std::{sync::mpsc, thread};

use arboard::Clipboard;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState},
    Frame,
};

use crate::cloudflare::{
    api::{self, DnsRecord, Zone},
    config::{self, AccountWithSecret},
};

use super::mouse;
use super::widgets::*;

const TYPE_FILTERS: &[&str] = &["All", "A", "AAAA", "CNAME", "MX", "TXT", "NS", "SRV", "CAA"];
/// Record types selectable when creating/editing a record (no "All").
const RECORD_TYPES: &[&str] = &["A", "AAAA", "CNAME", "MX", "TXT", "NS", "SRV", "CAA"];
/// CAA `tag` values Cloudflare accepts.
const CAA_TAGS: &[&str] = &["issue", "issuewild", "iodef"];

fn is_proxiable(type_name: &str) -> bool {
    matches!(type_name, "A" | "AAAA" | "CNAME")
}

fn fmt_ttl(ttl: i64) -> String {
    if ttl == 1 {
        "Auto".to_string()
    } else {
        ttl.to_string()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Accounts,
    Records,
}

// ── Accounts tab ─────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum AccField {
    Table,
    Label,
    ApiToken,
    BtnSave,
    BtnNew,
    BtnDelete,
    BtnTest,
}

struct AccountsTab {
    selected: Option<usize>, // index into cfg.accounts; None = creating new
    table_idx: usize,
    label: Input,
    api_token: Input,
    field: AccField,
}

impl AccountsTab {
    fn new() -> Self {
        Self { selected: None, table_idx: 0, label: Input::default(), api_token: Input::default(), field: AccField::Table }
    }

    fn next_field(&mut self) {
        self.field = match self.field {
            AccField::Table => AccField::Label,
            AccField::Label => AccField::ApiToken,
            AccField::ApiToken => AccField::BtnSave,
            AccField::BtnSave => AccField::BtnNew,
            AccField::BtnNew => AccField::BtnDelete,
            AccField::BtnDelete => AccField::BtnTest,
            AccField::BtnTest => AccField::Table,
        };
    }

    fn prev_field(&mut self) {
        self.field = match self.field {
            AccField::Table => AccField::BtnTest,
            AccField::Label => AccField::Table,
            AccField::ApiToken => AccField::Label,
            AccField::BtnSave => AccField::ApiToken,
            AccField::BtnNew => AccField::BtnSave,
            AccField::BtnDelete => AccField::BtnNew,
            AccField::BtnTest => AccField::BtnDelete,
        };
    }

    fn clear_form(&mut self) {
        self.selected = None;
        self.label = Input::default();
        self.api_token = Input::default();
    }
}

// ── Records tab ──────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum RecField {
    Account,
    Zone,
    BtnFetch,
    Search,
    TypeFilter,
    BtnFetchAll,
    Table,
    BtnAddRecords,
}

struct RecordsTab {
    account_input: Input,
    account_dropdown_open: bool,
    account_idx: usize,
    zone: Input,
    zone_dropdown_open: bool,
    zone_idx: usize,
    known_zones: Vec<Zone>,
    search: Input,
    type_filter_idx: usize,
    rows: Vec<DnsRecord>,
    selected_row: usize,
    field: RecField,
    modal: Option<RecordModal>,
}

impl RecordsTab {
    fn new() -> Self {
        Self {
            account_input: Input::default(),
            account_dropdown_open: false,
            account_idx: 0,
            zone: Input::default(),
            zone_dropdown_open: false,
            zone_idx: 0,
            known_zones: Vec::new(),
            search: Input::default(),
            type_filter_idx: 0,
            rows: Vec::new(),
            selected_row: 0,
            field: RecField::Account,
            modal: None,
        }
    }

    fn next_field(&mut self) {
        self.field = match self.field {
            RecField::Account => RecField::Zone,
            RecField::Zone => RecField::BtnFetch,
            RecField::BtnFetch => RecField::Search,
            RecField::Search => RecField::TypeFilter,
            RecField::TypeFilter => RecField::BtnFetchAll,
            RecField::BtnFetchAll => RecField::Table,
            RecField::Table => RecField::BtnAddRecords,
            RecField::BtnAddRecords => RecField::Account,
        };
    }

    fn prev_field(&mut self) {
        self.field = match self.field {
            RecField::Account => RecField::BtnAddRecords,
            RecField::Zone => RecField::Account,
            RecField::BtnFetch => RecField::Zone,
            RecField::Search => RecField::BtnFetch,
            RecField::TypeFilter => RecField::Search,
            RecField::BtnFetchAll => RecField::TypeFilter,
            RecField::Table => RecField::BtnFetchAll,
            RecField::BtnAddRecords => RecField::Table,
        };
    }

    /// The zone id for whatever's currently typed/picked in the Zone field,
    /// resolved against `known_zones` (fetched alongside the account).
    fn zone_id_for(&self, name: &str) -> Option<String> {
        self.known_zones.iter().find(|z| z.name == name).map(|z| z.id.clone())
    }
}

// ── Record modals ────────────────────────────────────────────────────────
enum RecordModal {
    Add(Box<AddModal>),
    Edit(Box<EditModal>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AddField {
    Zone,
    Type,
    Name,
    Priority,
    Weight,
    Port,
    Flags,
    Tag,
    Proxied,
    Value,
    Ttl,
    BtnStage,
    PendingList,
    BtnSaveAll,
    BtnCancel,
}

/// The fields relevant to `type_name`, in tab order. Different record
/// types need different extra fields (MX needs Priority; SRV also needs
/// Weight/Port; CAA needs Flags/Tag instead; A/AAAA/CNAME get a Proxied
/// toggle) — this is what makes the modal reshape itself around the
/// selected type.
fn add_active_fields(type_name: &str) -> Vec<AddField> {
    let mut v = vec![AddField::Zone, AddField::Type, AddField::Name];
    match type_name {
        "MX" => v.push(AddField::Priority),
        "SRV" => v.extend([AddField::Priority, AddField::Weight, AddField::Port]),
        "CAA" => v.extend([AddField::Flags, AddField::Tag]),
        _ => {}
    }
    v.push(AddField::Value);
    v.push(AddField::Ttl);
    if is_proxiable(type_name) {
        v.push(AddField::Proxied);
    }
    v.extend([AddField::BtnStage, AddField::PendingList, AddField::BtnSaveAll, AddField::BtnCancel]);
    v
}

struct AddModal {
    zone: Input,
    zone_dropdown_open: bool,
    zone_idx: usize,
    type_idx: usize,
    name: Input,
    priority: Input,
    weight: Input,
    port: Input,
    flags: Input,
    tag_idx: usize,
    proxied: bool,
    value: Input,
    ttl: Input,
    pending: Vec<api::DnsRecordInput>,
    pending_idx: usize,
    field: AddField,
    /// Set after the first Esc/Cancel while there are staged-but-unsaved
    /// records, so a second Esc/Cancel is required to actually discard them
    /// — "+ Add to list" only stages a record, it doesn't save it, and
    /// that's an easy thing to mistake for "done".
    confirm_discard: bool,
}

impl AddModal {
    fn new(zone: String) -> Self {
        let field = if zone.is_empty() { AddField::Zone } else { AddField::Type };
        Self {
            zone: Input::new(&zone),
            zone_dropdown_open: false,
            zone_idx: 0,
            type_idx: 0,
            name: Input::default(),
            priority: Input::new("10"),
            weight: Input::new("10"),
            port: Input::new("443"),
            flags: Input::new("0"),
            tag_idx: 0,
            proxied: true,
            value: Input::default(),
            ttl: Input::new("1"),
            pending: Vec::new(),
            pending_idx: 0,
            field,
            confirm_discard: false,
        }
    }

    fn current_type(&self) -> &'static str {
        RECORD_TYPES[self.type_idx]
    }

    fn next_field(&mut self) {
        let fields = add_active_fields(self.current_type());
        let idx = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = fields[(idx + 1) % fields.len()];
    }

    fn prev_field(&mut self) {
        let fields = add_active_fields(self.current_type());
        let idx = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = fields[(idx + fields.len() - 1) % fields.len()];
    }

    fn clear_inputs(&mut self) {
        self.name = Input::default();
        self.priority = Input::new("10");
        self.weight = Input::new("10");
        self.port = Input::new("443");
        self.flags = Input::new("0");
        self.tag_idx = 0;
        self.proxied = true;
        self.value = Input::default();
        self.ttl = Input::new("1");
        // type_idx deliberately kept: staging several records of the same
        // type in a row is the common case.
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditField {
    Type,
    Name,
    Priority,
    Weight,
    Port,
    Flags,
    Tag,
    Proxied,
    Value,
    Ttl,
    BtnUpdate,
    BtnDelete,
    BtnCancel,
}

fn edit_active_fields(type_name: &str) -> Vec<EditField> {
    let mut v = vec![EditField::Type, EditField::Name];
    match type_name {
        "MX" => v.push(EditField::Priority),
        "SRV" => v.extend([EditField::Priority, EditField::Weight, EditField::Port]),
        "CAA" => v.extend([EditField::Flags, EditField::Tag]),
        _ => {}
    }
    v.push(EditField::Value);
    v.push(EditField::Ttl);
    if is_proxiable(type_name) {
        v.push(EditField::Proxied);
    }
    v.extend([EditField::BtnUpdate, EditField::BtnDelete, EditField::BtnCancel]);
    v
}

struct EditModal {
    original: DnsRecord,
    type_idx: usize,
    name: Input,
    priority: Input,
    weight: Input,
    port: Input,
    flags: Input,
    tag_idx: usize,
    proxied: bool,
    value: Input,
    ttl: Input,
    field: EditField,
}

impl EditModal {
    fn new(r: &DnsRecord) -> Self {
        let type_idx = RECORD_TYPES.iter().position(|t| *t == r.type_).unwrap_or(0);
        let tag_idx = r.tag.as_deref().and_then(|t| CAA_TAGS.iter().position(|c| *c == t)).unwrap_or(0);
        Self {
            original: r.clone(),
            type_idx,
            name: Input::new(&r.subdomain),
            priority: Input::new(&r.priority.unwrap_or(10).to_string()),
            weight: Input::new(&r.weight.unwrap_or(10).to_string()),
            port: Input::new(&r.port.unwrap_or(443).to_string()),
            flags: Input::new(&r.flags.unwrap_or(0).to_string()),
            tag_idx,
            proxied: r.proxied,
            value: Input::new(&r.value),
            ttl: Input::new(&r.ttl.to_string()),
            field: EditField::Type,
        }
    }

    fn current_type(&self) -> &'static str {
        RECORD_TYPES[self.type_idx]
    }

    fn next_field(&mut self) {
        let fields = edit_active_fields(self.current_type());
        let idx = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = fields[(idx + 1) % fields.len()];
    }

    fn prev_field(&mut self) {
        let fields = edit_active_fields(self.current_type());
        let idx = fields.iter().position(|f| *f == self.field).unwrap_or(0);
        self.field = fields[(idx + fields.len() - 1) % fields.len()];
    }
}

fn parse_i64_field(input: &Input, field_name: &str) -> Result<i64, String> {
    input.value().trim().parse().map_err(|_| format!("{field_name} must be a number"))
}

/// Builds the API payload for the record currently being staged/edited in
/// a modal, given its selected type and the type-specific input widgets.
/// `type_idx`/`tag_idx` select from `RECORD_TYPES`/`CAA_TAGS`.
#[allow(clippy::too_many_arguments)]
fn parse_modal_record(
    type_idx: usize,
    name: &Input,
    priority: &Input,
    weight: &Input,
    port: &Input,
    flags: &Input,
    tag_idx: usize,
    proxied: bool,
    value: &Input,
    ttl: &Input,
) -> Result<api::DnsRecordInput, String> {
    let ttl_val: i64 = ttl.value().trim().parse().map_err(|_| "TTL must be a number (1 = Automatic, or seconds)".to_string())?;
    if ttl_val < 1 {
        return Err("TTL must be 1 (Automatic) or a positive number of seconds — Cloudflare's actual minimum depends on your plan (typically 60s), and is enforced by the API itself".to_string());
    }
    let value_val = value.value().trim().to_string();
    if value_val.is_empty() {
        return Err("Value is required".to_string());
    }
    let type_val = RECORD_TYPES[type_idx].to_string();
    let mut input = api::DnsRecordInput {
        type_: type_val.clone(),
        subdomain: name.value().trim().to_string(),
        value: value_val,
        ttl: ttl_val,
        proxied: is_proxiable(&type_val) && proxied,
        ..Default::default()
    };
    match type_val.as_str() {
        "MX" => input.priority = Some(parse_i64_field(priority, "Priority")?),
        "SRV" => {
            input.priority = Some(parse_i64_field(priority, "Priority")?);
            input.weight = Some(parse_i64_field(weight, "Weight")?);
            input.port = Some(parse_i64_field(port, "Port")?);
        }
        "CAA" => {
            input.flags = Some(parse_i64_field(flags, "Flags")?);
            input.tag = Some(CAA_TAGS[tag_idx].to_string());
        }
        _ => {}
    }
    Ok(input)
}

enum CfMsg {
    Log(bool, String),
    RecordsResult(Result<(Vec<DnsRecord>, usize, usize), String>),
    ZonesResult(Result<Vec<Zone>, String>),
}

pub struct CloudflareScreen {
    tab: Tab,
    accounts_tab: AccountsTab,
    records_tab: RecordsTab,
    modal: Option<(String, String)>,
    cfg: config::Config,
    history: Vec<(bool, String)>,
    /// Lines scrolled up from the newest entry in the History panel — see `widgets::draw_history`.
    history_scroll: u16,
    tx: mpsc::Sender<CfMsg>,
    rx: mpsc::Receiver<CfMsg>,
}

impl CloudflareScreen {
    pub fn new() -> Self {
        let cfg = config::load().unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        Self {
            tab: Tab::Accounts,
            accounts_tab: AccountsTab::new(),
            records_tab: RecordsTab::new(),
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
                CfMsg::Log(ok, line) => self.history.push((ok, line)),
                CfMsg::RecordsResult(Ok((records, zones, skipped))) => {
                    self.history.push((
                        true,
                        format!("Fetched {} record(s) across {} zone(s) ({} skipped)", records.len(), zones, skipped),
                    ));
                    self.records_tab.rows = records;
                    self.records_tab.selected_row = 0;
                }
                CfMsg::RecordsResult(Err(e)) => {
                    self.history.push((false, one_line(&e)));
                }
                CfMsg::ZonesResult(Ok(zones)) => {
                    self.records_tab.known_zones = zones;
                }
                CfMsg::ZonesResult(Err(e)) => {
                    self.history.push((false, format!("Failed to list zones: {}", one_line(&e))));
                }
            }
        }
    }

    fn account_by_label(&self, label: &str) -> Option<AccountWithSecret> {
        let a = self.cfg.accounts.iter().find(|a| a.label == label)?;
        self.cfg.with_secret(&a.id)
    }

    /// Records currently visible after Account / Type / Search filters are
    /// applied over whatever is loaded in `rows`. This is what makes the
    /// "Fetch All Accounts" + Search combo behave like a global search.
    fn visible_rows(&self) -> Vec<&DnsRecord> {
        let rt = &self.records_tab;
        let account_filter = rt.account_input.value().trim();
        let account_active = !account_filter.is_empty() && self.cfg.accounts.iter().any(|a| a.label == account_filter);
        let type_filter = TYPE_FILTERS[rt.type_filter_idx];
        let search = rt.search.value().trim().to_lowercase();

        rt.rows
            .iter()
            .filter(|r| {
                if account_active && r.account_label != account_filter {
                    return false;
                }
                if type_filter != "All" && r.type_ != type_filter {
                    return false;
                }
                if !search.is_empty() {
                    let haystack = format!("{} {} {} {}", r.domain, r.subdomain, r.type_, r.value).to_lowercase();
                    if !haystack.contains(&search) {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    fn spawn_zones_refresh(&self, account: AccountWithSecret) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = api::get_zones(&account);
            let _ = tx.send(CfMsg::ZonesResult(result));
        });
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

        match key.code {
            KeyCode::Esc
                if !self.records_tab.account_dropdown_open
                    && !self.records_tab.zone_dropdown_open
                    && self.records_tab.modal.is_none() =>
            {
                return true
            }
            KeyCode::F(1) if self.records_tab.modal.is_none() => {
                self.tab = Tab::Accounts;
                return false;
            }
            KeyCode::F(2) if self.records_tab.modal.is_none() => {
                self.tab = Tab::Records;
                self.trigger_fetch_all_if_empty();
                return false;
            }
            _ => {}
        }

        match self.tab {
            Tab::Accounts => self.handle_accounts_key(key),
            Tab::Records => self.handle_records_key(key),
        }
        false
    }

    pub fn handle_mouse(&mut self, me: MouseEvent, area: Rect) {
        if self.modal.is_some() {
            // A stray error dialog has no mouse affordance of its own, so
            // without this, one failed action (e.g. "Fetch" with no
            // account picked) silently eats every click for the rest of
            // the session. Any click dismisses it, same as Enter/Esc/q.
            if mouse::left_click(&me).is_some() {
                self.modal = None;
            }
            return;
        }
        if self.tab == Tab::Records && self.records_tab.modal.is_some() {
            // The Add/Edit Record modal is centered on the *full* screen
            // area, not the sub-area below the tab bar (same as `draw()`
            // renders it), so it needs that full `area` too or every
            // click would land a row too high.
            self.handle_record_modal_mouse(me, area);
            return;
        }
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(area);

        if let Some((x, y)) = mouse::left_click(&me) {
            if let Some(i) = mouse::label_row_hit(x, y, chunks[0], &["F1 Accounts", "F2 Records"]) {
                self.tab = if i == 0 { Tab::Accounts } else { Tab::Records };
                if i == 1 {
                    self.trigger_fetch_all_if_empty();
                }
                return;
            }
        }

        match self.tab {
            Tab::Accounts => self.handle_accounts_mouse(me, chunks[1]),
            Tab::Records => self.handle_records_mouse(me, chunks[1]),
        }
    }

    fn handle_accounts_mouse(&mut self, me: MouseEvent, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(12), Constraint::Length(6)])
            .split(area);

        if let Some(delta) = mouse::scroll_delta(&me) {
            let n = self.cfg.accounts.len();
            if n > 0 && mouse::in_rect(chunks[0], me.column, me.row) {
                if delta < 0 && self.accounts_tab.table_idx > 0 {
                    self.accounts_tab.table_idx -= 1;
                } else if delta > 0 && self.accounts_tab.table_idx + 1 < n {
                    self.accounts_tab.table_idx += 1;
                }
            } else if mouse::in_rect(chunks[2], me.column, me.row) {
                self.history_scroll = if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };

        if let Some(idx) = mouse::table_row_hit(x, y, chunks[0], 1, self.cfg.accounts.len(), self.accounts_tab.table_idx) {
            self.accounts_tab.table_idx = idx;
            if let Some(a) = self.cfg.accounts.get(idx) {
                self.accounts_tab.selected = Some(idx);
                self.accounts_tab.label = Input::new(&a.label);
                self.accounts_tab.api_token = Input::default();
            }
            return;
        }

        let form_inner = mouse::block_inner(mouse::block_inner(chunks[1]));
        let rows2 = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // [0] Label
                Constraint::Length(1),
                Constraint::Length(1), // [2] API Token
                Constraint::Length(1),
                Constraint::Length(1), // [4] hint
                Constraint::Length(1),
                Constraint::Length(1), // [6] buttons
            ])
            .split(form_inner);

        if let Some(i) = mouse::button_row_hit(x, y, rows2[6], &["Save", "New", "Delete", "Test Token"]) {
            match i {
                0 => self.trigger_save_account(),
                1 => self.accounts_tab.clear_form(),
                2 => self.trigger_delete_account(),
                _ => self.trigger_test_token(),
            }
            return;
        }
        if mouse::in_rect(rows2[0], x, y) {
            self.accounts_tab.field = AccField::Label;
        } else if mouse::in_rect(rows2[2], x, y) {
            self.accounts_tab.field = AccField::ApiToken;
        }
    }

    fn handle_records_mouse(&mut self, me: MouseEvent, area: Rect) {
        if self.records_tab.modal.is_some() {
            return;
        }

        // Must come before any hit-testing below: with a dropdown floating
        // on screen, a click anywhere dismisses it (matching Esc) rather
        // than falling through to whatever field/row is underneath it, and
        // a wheel scroll is ignored rather than silently scrolling the
        // records table or history panel *behind* the dropdown — the same
        // gap keyboard Up/Down doesn't have, since that's already scoped to
        // the dropdown by `handle_account_dropdown_key`/`handle_zone_dropdown_key`.
        if self.records_tab.account_dropdown_open || self.records_tab.zone_dropdown_open {
            if mouse::left_click(&me).is_some() {
                self.records_tab.account_dropdown_open = false;
                self.records_tab.zone_dropdown_open = false;
            }
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(7), Constraint::Length(7), Constraint::Min(6), Constraint::Length(6), Constraint::Length(7)])
            .split(area);

        if let Some(delta) = mouse::scroll_delta(&me) {
            let n = self.visible_rows().len();
            if n > 0 && mouse::in_rect(chunks[2], me.column, me.row) {
                if delta < 0 && self.records_tab.selected_row > 0 {
                    self.records_tab.selected_row -= 1;
                } else if delta > 0 && self.records_tab.selected_row + 1 < n {
                    self.records_tab.selected_row += 1;
                }
            } else if mouse::in_rect(chunks[4], me.column, me.row) {
                self.history_scroll = if delta < 0 { self.history_scroll.saturating_add(3) } else { self.history_scroll.saturating_sub(3) };
            }
            return;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return };

        // ── Fetch box ──
        let top_inner = mouse::block_inner(chunks[0]);
        let top_rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
            .split(top_inner);
        if mouse::in_rect(top_rows[0], x, y) {
            self.records_tab.field = RecField::Account;
            if !self.cfg.accounts.is_empty() {
                self.records_tab.account_idx = 0;
                self.records_tab.account_dropdown_open = true;
            }
            return;
        }
        if mouse::in_rect(top_rows[2], x, y) {
            let fw2 = top_rows[2].width.saturating_sub(12 + 2 + 16) as usize;
            let btn_x = top_rows[2].x + 12 + fw2 as u16 + 2;
            if x >= btn_x {
                self.trigger_fetch_records();
            } else {
                self.records_tab.field = RecField::Zone;
                if !self.records_tab.known_zones.is_empty() {
                    self.records_tab.zone_idx = 0;
                    self.records_tab.zone_dropdown_open = true;
                }
            }
            return;
        }

        // ── Search box ──
        let search_inner = mouse::block_inner(chunks[1]);
        let search_rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
            .split(search_inner);
        if mouse::in_rect(search_rows[0], x, y) {
            self.records_tab.field = RecField::Search;
            return;
        }
        if mouse::in_rect(search_rows[2], x, y) {
            let type_label = TYPE_FILTERS[self.records_tab.type_filter_idx];
            let type_btn_w = type_label.chars().count() as u16 + 4;
            let type_btn_x = search_rows[2].x + 6;
            let fetchall_btn_x = type_btn_x + type_btn_w + 10; // "  (\u{2190}/\u{2192})   " gap
            let fetchall_btn_w = "Fetch All Accounts".chars().count() as u16 + 4;
            if x >= type_btn_x && x < type_btn_x + type_btn_w {
                self.records_tab.type_filter_idx = (self.records_tab.type_filter_idx + 1) % TYPE_FILTERS.len();
            } else if x >= fetchall_btn_x && x < fetchall_btn_x + fetchall_btn_w {
                self.trigger_fetch_all_accounts();
            } else {
                self.records_tab.field = RecField::TypeFilter;
            }
            return;
        }

        // ── Records table ──
        let visible_len = self.visible_rows().len();
        if let Some(idx) = mouse::table_row_hit(x, y, chunks[2], 1, visible_len, self.records_tab.selected_row) {
            self.records_tab.field = RecField::Table;
            self.records_tab.selected_row = idx;
            let record = self.visible_rows()[idx].clone();
            self.records_tab.modal = Some(RecordModal::Edit(Box::new(EditModal::new(&record))));
            return;
        }

        // ── Actions box ──
        let action_inner = mouse::block_inner(chunks[3]);
        let action_rows = Layout::default().direction(Direction::Vertical).margin(1).constraints([Constraint::Length(1), Constraint::Length(1)]).split(action_inner);
        if mouse::button_row_hit(x, y, action_rows[0], &["Add Record(s)"]).is_some() {
            self.trigger_open_add_modal();
        }
    }

    fn handle_record_modal_mouse(&mut self, me: MouseEvent, area: Rect) {
        match self.records_tab.modal.take() {
            Some(RecordModal::Add(mut m)) => {
                let close = self.handle_add_modal_mouse(&mut m, me, area);
                if !close {
                    self.records_tab.modal = Some(RecordModal::Add(m));
                }
            }
            Some(RecordModal::Edit(mut m)) => {
                let close = self.handle_edit_modal_mouse(&mut m, me, area);
                if !close {
                    self.records_tab.modal = Some(RecordModal::Edit(m));
                }
            }
            None => {}
        }
    }

    /// Sets `m.field` to whatever was clicked, then — for buttons and
    /// toggles — replays the *keyboard* Enter handler instead of
    /// reimplementing "stage"/"save all"/network-thread logic a second
    /// time here. Row offsets mirror `draw_add_modal` exactly (see its
    /// comments); this must stay in sync with that if the form changes.
    fn handle_add_modal_mouse(&mut self, m: &mut AddModal, me: MouseEvent, area: Rect) -> bool {
        let width = 92u16.min(area.width.saturating_sub(4));
        let height = 23u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);
        let inner = mouse::block_inner(modal_area);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // 0 Zone
                Constraint::Length(1),
                Constraint::Length(1), // 2 Type/Name/TTL(/Proxied)
                Constraint::Length(1),
                Constraint::Length(1), // 4 type-specific
                Constraint::Length(1),
                Constraint::Length(1), // 6 Value
                Constraint::Length(1),
                Constraint::Length(1), // 8 stage button
                Constraint::Length(1),
                Constraint::Length(1), // 10 Pending label
                Constraint::Min(3),    // 11 pending list
                Constraint::Length(1),
                Constraint::Length(1), // 13 Save All / Cancel
                Constraint::Length(1), // 14 hint
            ])
            .split(inner);

        if m.zone_dropdown_open {
            if let Some((x, y)) = mouse::left_click(&me) {
                let matches = filtered_zones(&self.records_tab.known_zones, m.zone.value());
                let dd_x = rows[0].x + 6;
                let dd_area = Rect { x: dd_x, y: rows[0].y + 1, width: 34u16.min(area.width.saturating_sub(dd_x)), height: (matches.len() as u16 + 2).min(10) };
                if let Some(idx) = mouse::table_row_hit(x, y, dd_area, 0, matches.len(), m.zone_idx) {
                    if let Some(z) = matches.get(idx) {
                        m.zone = Input::new(z);
                    }
                }
                m.zone_dropdown_open = false;
            }
            return false;
        }

        if let Some(delta) = mouse::scroll_delta(&me) {
            if !m.pending.is_empty() && mouse::in_rect(rows[11], me.column, me.row) {
                if delta < 0 && m.pending_idx > 0 {
                    m.pending_idx -= 1;
                } else if delta > 0 && m.pending_idx + 1 < m.pending.len() {
                    m.pending_idx += 1;
                }
            }
            return false;
        }

        let Some((x, y)) = mouse::left_click(&me) else { return false };
        let mut activate = false;

        if mouse::in_rect(rows[0], x, y) {
            m.field = AddField::Zone;
            activate = true;
        } else if let Some(idx) = mouse::plain_row_hit(x, y, rows[11], m.pending.len()) {
            m.field = AddField::PendingList;
            m.pending_idx = idx;
        } else if mouse::button_row_hit(x, y, rows[8], &["+ Add to list"]).is_some() {
            m.field = AddField::BtnStage;
            activate = true;
        } else if let Some(i) = mouse::button_row_hit(x, y, rows[13], &["Save All", "Cancel"]) {
            m.field = if i == 0 { AddField::BtnSaveAll } else { AddField::BtnCancel };
            activate = true;
        } else if mouse::in_rect(rows[6], x, y) {
            m.field = AddField::Value;
        } else if mouse::in_rect(rows[2], x, y) {
            let type_w = m.current_type().chars().count() as u16 + 4;
            let type_btn_x = rows[2].x + 6;
            let name_input_x = type_btn_x + type_w + 9 + 6;
            let ttl_input_x = name_input_x + 20 + 2 + 5;
            let proxied_label_x = ttl_input_x + 8 + 2;
            if x >= type_btn_x && x < type_btn_x + type_w {
                m.field = AddField::Type;
                activate = true;
            } else if is_proxiable(m.current_type()) && x >= proxied_label_x + 9 {
                m.field = AddField::Proxied;
                activate = true;
            } else if x >= ttl_input_x {
                m.field = AddField::Ttl;
            } else if x >= name_input_x {
                m.field = AddField::Name;
            }
        } else if mouse::in_rect(rows[4], x, y) {
            match m.current_type() {
                "MX" => m.field = AddField::Priority,
                "SRV" => {
                    let p_end = rows[4].x + 10 + 6;
                    let w_start = p_end + 2 + 8;
                    let w_end = w_start + 6;
                    let port_start = w_end + 2 + 6;
                    if x >= port_start {
                        m.field = AddField::Port;
                    } else if x >= w_start {
                        m.field = AddField::Weight;
                    } else {
                        m.field = AddField::Priority;
                    }
                }
                "CAA" => {
                    let tag_x = rows[4].x + 7 + 4 + 12 + 5;
                    let tag_w = CAA_TAGS[m.tag_idx].chars().count() as u16 + 4;
                    if x >= tag_x && x < tag_x + tag_w {
                        m.field = AddField::Tag;
                        activate = true;
                    } else {
                        m.field = AddField::Flags;
                    }
                }
                _ => {}
            }
        }

        if activate {
            return self.handle_add_modal_key(m, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        // Mirrors `handle_add_modal_key`'s `is_discard_attempt` guard: only
        // a Cancel click can be a discard attempt, and that always sets
        // `activate` above, so every click that reaches here is — like any
        // non-Esc/non-Cancel keypress — proof the user isn't confirming a
        // pending discard, and should disarm it.
        m.confirm_discard = false;
        false
    }

    /// Same approach as `handle_add_modal_mouse` — set focus, then replay
    /// a keyboard Enter for anything that acts rather than just focuses.
    /// Row offsets mirror `draw_edit_modal`.
    fn handle_edit_modal_mouse(&mut self, m: &mut EditModal, me: MouseEvent, area: Rect) -> bool {
        let width = 92u16.min(area.width.saturating_sub(4));
        let height = 15u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);
        let inner = mouse::block_inner(modal_area);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // 0 Type/Name/TTL(/Proxied)
                Constraint::Length(1),
                Constraint::Length(1), // 2 type-specific
                Constraint::Length(1),
                Constraint::Length(1), // 4 Value
                Constraint::Length(1),
                Constraint::Length(1), // 6 buttons
                Constraint::Length(1),
                Constraint::Length(1), // 8 hint
                Constraint::Min(0),
            ])
            .split(inner);

        let Some((x, y)) = mouse::left_click(&me) else { return false };
        let mut activate = false;

        if let Some(i) = mouse::button_row_hit(x, y, rows[6], &["Update", "Delete", "Cancel"]) {
            m.field = match i {
                0 => EditField::BtnUpdate,
                1 => EditField::BtnDelete,
                _ => EditField::BtnCancel,
            };
            activate = true;
        } else if mouse::in_rect(rows[4], x, y) {
            m.field = EditField::Value;
        } else if mouse::in_rect(rows[0], x, y) {
            let type_w = m.current_type().chars().count() as u16 + 4;
            let type_btn_x = rows[0].x + 6;
            let name_input_x = type_btn_x + type_w + 9 + 6;
            let ttl_input_x = name_input_x + 20 + 2 + 5;
            let proxied_label_x = ttl_input_x + 8 + 2;
            if x >= type_btn_x && x < type_btn_x + type_w {
                m.field = EditField::Type;
                activate = true;
            } else if is_proxiable(m.current_type()) && x >= proxied_label_x + 9 {
                m.field = EditField::Proxied;
                activate = true;
            } else if x >= ttl_input_x {
                m.field = EditField::Ttl;
            } else if x >= name_input_x {
                m.field = EditField::Name;
            }
        } else if mouse::in_rect(rows[2], x, y) {
            match m.current_type() {
                "MX" => m.field = EditField::Priority,
                "SRV" => {
                    let p_end = rows[2].x + 10 + 6;
                    let w_start = p_end + 2 + 8;
                    let w_end = w_start + 6;
                    let port_start = w_end + 2 + 6;
                    if x >= port_start {
                        m.field = EditField::Port;
                    } else if x >= w_start {
                        m.field = EditField::Weight;
                    } else {
                        m.field = EditField::Priority;
                    }
                }
                "CAA" => {
                    let tag_x = rows[2].x + 7 + 4 + 12 + 5;
                    let tag_w = CAA_TAGS[m.tag_idx].chars().count() as u16 + 4;
                    if x >= tag_x && x < tag_x + tag_w {
                        m.field = EditField::Tag;
                        activate = true;
                    } else {
                        m.field = EditField::Flags;
                    }
                }
                _ => {}
            }
        }

        if activate {
            return self.handle_edit_modal_key(m, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        false
    }

    // ── Accounts ──────────────────────────────────────────────────────
    fn handle_accounts_key(&mut self, key: KeyEvent) {
        let n = self.cfg.accounts.len();
        let at = &mut self.accounts_tab;

        if at.field == AccField::Table && n > 0 {
            match key.code {
                KeyCode::Up => {
                    if at.table_idx > 0 {
                        at.table_idx -= 1;
                    }
                    return;
                }
                KeyCode::Down => {
                    if at.table_idx + 1 < n {
                        at.table_idx += 1;
                    }
                    return;
                }
                KeyCode::Enter => {
                    if let Some(a) = self.cfg.accounts.get(at.table_idx) {
                        at.selected = Some(at.table_idx);
                        at.label = Input::new(&a.label);
                        at.api_token = Input::default();
                    }
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Tab => at.next_field(),
            KeyCode::BackTab => at.prev_field(),
            KeyCode::Up => at.prev_field(),
            KeyCode::Down => at.next_field(),
            KeyCode::Enter => match at.field {
                AccField::BtnSave => self.trigger_save_account(),
                AccField::BtnNew => self.accounts_tab.clear_form(),
                AccField::BtnDelete => self.trigger_delete_account(),
                AccField::BtnTest => self.trigger_test_token(),
                _ => self.accounts_tab.next_field(),
            },
            KeyCode::Char(c) => match at.field {
                AccField::Label => at.label.insert(c),
                AccField::ApiToken => at.api_token.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match at.field {
                AccField::Label => at.label.backspace(),
                AccField::ApiToken => at.api_token.backspace(),
                _ => {}
            },
            KeyCode::Delete => match at.field {
                AccField::Label => at.label.delete(),
                AccField::ApiToken => at.api_token.delete(),
                _ => {}
            },
            KeyCode::Left => match at.field {
                AccField::Label => at.label.left(),
                AccField::ApiToken => at.api_token.left(),
                _ => {}
            },
            KeyCode::Right => match at.field {
                AccField::Label => at.label.right(),
                AccField::ApiToken => at.api_token.right(),
                _ => {}
            },
            KeyCode::Home => match at.field {
                AccField::Label => at.label.home(),
                AccField::ApiToken => at.api_token.home(),
                _ => {}
            },
            KeyCode::End => match at.field {
                AccField::Label => at.label.end_of_line(),
                AccField::ApiToken => at.api_token.end_of_line(),
                _ => {}
            },
            _ => {}
        }
    }

    fn trigger_save_account(&mut self) {
        let label = self.accounts_tab.label.value().trim().to_string();
        let api_token = self.accounts_tab.api_token.value().trim().to_string();
        if label.is_empty() {
            self.modal = Some(("Error".into(), "Label is required".into()));
            return;
        }
        let existing_id = self.accounts_tab.selected.and_then(|i| self.cfg.accounts.get(i)).map(|a| a.id.clone());
        if existing_id.is_some() && api_token.is_empty() {
            self.modal = Some(("Error".into(), "API Token is required to update (re-enter it)".into()));
            return;
        }
        if existing_id.is_none() && api_token.is_empty() {
            self.modal = Some(("Error".into(), "API Token is required".into()));
            return;
        }

        match self.cfg.upsert_account(existing_id.as_deref(), label, api_token) {
            Ok(_) => {
                if let Err(e) = config::save(&self.cfg) {
                    self.modal = Some(("Error".into(), e.to_string()));
                    return;
                }
                self.accounts_tab.clear_form();
                self.history.push((true, "Account saved".into()));
            }
            Err(e) => self.modal = Some(("Error".into(), e.to_string())),
        }
    }

    fn trigger_delete_account(&mut self) {
        let Some(idx) = self.accounts_tab.selected.or(if self.cfg.accounts.is_empty() { None } else { Some(self.accounts_tab.table_idx) }) else {
            self.modal = Some(("Error".into(), "No account selected".into()));
            return;
        };
        let Some(a) = self.cfg.accounts.get(idx).cloned() else {
            return;
        };
        self.cfg.delete_account(&a.id);
        match config::save(&self.cfg) {
            Ok(_) => {
                self.accounts_tab.clear_form();
                self.accounts_tab.table_idx = 0;
                self.history.push((true, format!("Account '{}' deleted", a.label)));
            }
            Err(e) => self.modal = Some(("Error".into(), e.to_string())),
        }
    }

    fn trigger_test_token(&mut self) {
        let api_token = if !self.accounts_tab.api_token.value().trim().is_empty() {
            self.accounts_tab.api_token.value().trim().to_string()
        } else if let Some(idx) = self.accounts_tab.selected {
            self.cfg.accounts.get(idx).and_then(|a| self.cfg.with_secret(&a.id)).map(|a| a.api_token).unwrap_or_default()
        } else {
            String::new()
        };
        if api_token.is_empty() {
            self.modal = Some(("Error".into(), "API Token is required to test".into()));
            return;
        }
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = api::test_token(&api_token);
            if result.ok {
                let _ = tx.send(CfMsg::Log(true, format!("Token valid — sees {} zone(s)", result.zones_count)));
            } else {
                let _ = tx.send(CfMsg::Log(false, format!("Token test failed: {}", one_line(&result.message.unwrap_or_default()))));
            }
        });
    }

    // ── Records ───────────────────────────────────────────────────────
    fn handle_records_key(&mut self, key: KeyEvent) {
        if self.records_tab.modal.is_some() {
            self.handle_record_modal_key(key);
            return;
        }

        if self.records_tab.account_dropdown_open {
            self.handle_account_dropdown_key(key);
            return;
        }

        if self.records_tab.zone_dropdown_open {
            self.handle_zone_dropdown_key(key);
            return;
        }

        if self.records_tab.field == RecField::Table {
            let visible_len = self.visible_rows().len();
            if visible_len > 0 {
                match key.code {
                    KeyCode::Up => {
                        if self.records_tab.selected_row > 0 {
                            self.records_tab.selected_row -= 1;
                        }
                        return;
                    }
                    KeyCode::Down => {
                        if self.records_tab.selected_row + 1 < visible_len {
                            self.records_tab.selected_row += 1;
                        }
                        return;
                    }
                    KeyCode::Enter => {
                        let idx = self.records_tab.selected_row.min(visible_len - 1);
                        let record = self.visible_rows()[idx].clone();
                        self.records_tab.modal = Some(RecordModal::Edit(Box::new(EditModal::new(&record))));
                        return;
                    }
                    KeyCode::Char('y') => {
                        let idx = self.records_tab.selected_row.min(visible_len - 1);
                        let value = self.visible_rows()[idx].value.clone();
                        let ok = Clipboard::new().and_then(|mut c| c.set_text(value.clone())).is_ok();
                        self.history.push((ok, if ok { format!("Copied to clipboard: {value}") } else { "Couldn't access the clipboard".into() }));
                        return;
                    }
                    _ => {}
                }
            }
        }

        if self.records_tab.field == RecField::TypeFilter {
            match key.code {
                KeyCode::Left => {
                    let n = TYPE_FILTERS.len();
                    self.records_tab.type_filter_idx = (self.records_tab.type_filter_idx + n - 1) % n;
                    return;
                }
                KeyCode::Right => {
                    self.records_tab.type_filter_idx = (self.records_tab.type_filter_idx + 1) % TYPE_FILTERS.len();
                    return;
                }
                _ => {}
            }
        }

        let rt = &mut self.records_tab;
        match key.code {
            KeyCode::Tab => rt.next_field(),
            KeyCode::BackTab => rt.prev_field(),
            KeyCode::Up => rt.prev_field(),
            KeyCode::Down => rt.next_field(),
            KeyCode::Enter => match rt.field {
                RecField::Account => {
                    if !self.cfg.accounts.is_empty() {
                        self.records_tab.account_idx = 0;
                        self.records_tab.account_dropdown_open = true;
                    }
                }
                RecField::Zone => {
                    if !self.records_tab.known_zones.is_empty() {
                        self.records_tab.zone_idx = 0;
                        self.records_tab.zone_dropdown_open = true;
                    }
                }
                RecField::BtnFetch => self.trigger_fetch_records(),
                RecField::TypeFilter => {
                    self.records_tab.type_filter_idx = (self.records_tab.type_filter_idx + 1) % TYPE_FILTERS.len();
                }
                RecField::BtnFetchAll => self.trigger_fetch_all_accounts(),
                RecField::BtnAddRecords => self.trigger_open_add_modal(),
                _ => self.records_tab.next_field(),
            },
            KeyCode::Char(c) => match rt.field {
                RecField::Account => {
                    rt.account_input.insert(c);
                    rt.known_zones.clear();
                    if !self.cfg.accounts.is_empty() {
                        self.records_tab.account_dropdown_open = true;
                        self.records_tab.account_idx = 0;
                    }
                }
                RecField::Zone => {
                    rt.zone.insert(c);
                    if !rt.known_zones.is_empty() {
                        self.records_tab.zone_dropdown_open = true;
                        self.records_tab.zone_idx = 0;
                    }
                }
                RecField::Search => rt.search.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match rt.field {
                RecField::Account => {
                    rt.account_input.backspace();
                    rt.known_zones.clear();
                }
                RecField::Zone => rt.zone.backspace(),
                RecField::Search => rt.search.backspace(),
                _ => {}
            },
            KeyCode::Delete => match rt.field {
                RecField::Account => rt.account_input.delete(),
                RecField::Zone => rt.zone.delete(),
                RecField::Search => rt.search.delete(),
                _ => {}
            },
            KeyCode::Left => match rt.field {
                RecField::Account => rt.account_input.left(),
                RecField::Zone => rt.zone.left(),
                RecField::Search => rt.search.left(),
                _ => {}
            },
            KeyCode::Right => match rt.field {
                RecField::Account => rt.account_input.right(),
                RecField::Zone => rt.zone.right(),
                RecField::Search => rt.search.right(),
                _ => {}
            },
            KeyCode::Home => match rt.field {
                RecField::Account => rt.account_input.home(),
                RecField::Zone => rt.zone.home(),
                RecField::Search => rt.search.home(),
                _ => {}
            },
            KeyCode::End => match rt.field {
                RecField::Account => rt.account_input.end_of_line(),
                RecField::Zone => rt.zone.end_of_line(),
                RecField::Search => rt.search.end_of_line(),
                _ => {}
            },
            _ => {}
        }
    }

    fn handle_account_dropdown_key(&mut self, key: KeyEvent) {
        let mut committed_label: Option<String> = None;
        {
            let rt = &mut self.records_tab;
            match key.code {
                KeyCode::Esc => rt.account_dropdown_open = false,
                KeyCode::Tab => {
                    rt.account_dropdown_open = false;
                    rt.next_field();
                }
                KeyCode::BackTab => {
                    rt.account_dropdown_open = false;
                    rt.prev_field();
                }
                KeyCode::Up => {
                    if rt.account_idx > 0 {
                        rt.account_idx -= 1;
                    }
                }
                KeyCode::Down => {
                    let count = filtered_accounts(&self.cfg, rt.account_input.value()).len();
                    if rt.account_idx + 1 < count {
                        rt.account_idx += 1;
                    }
                }
                KeyCode::Enter => {
                    let matches = filtered_accounts(&self.cfg, rt.account_input.value());
                    if let Some(label) = matches.get(rt.account_idx) {
                        rt.account_input = Input::new(label);
                        rt.known_zones.clear();
                        committed_label = Some(label.clone());
                    }
                    rt.account_dropdown_open = false;
                }
                KeyCode::Char(c) => {
                    rt.account_input.insert(c);
                    rt.account_idx = 0;
                    rt.known_zones.clear();
                }
                KeyCode::Backspace => {
                    rt.account_input.backspace();
                    rt.account_idx = 0;
                    rt.known_zones.clear();
                }
                KeyCode::Delete => rt.account_input.delete(),
                KeyCode::Left => rt.account_input.left(),
                KeyCode::Right => rt.account_input.right(),
                KeyCode::Home => rt.account_input.home(),
                KeyCode::End => rt.account_input.end_of_line(),
                _ => {}
            }
        }
        if let Some(label) = committed_label {
            if let Some(account) = self.account_by_label(&label) {
                self.spawn_zones_refresh(account);
            }
            // Picking an account is "I want to work with this account now" —
            // fetch its records right away instead of making the user also
            // press Fetch, mirroring what a GUI tool would do on selection.
            self.trigger_fetch_records();
        }
    }

    fn handle_zone_dropdown_key(&mut self, key: KeyEvent) {
        let mut committed_zone = false;
        {
            let rt = &mut self.records_tab;
            let matches: Vec<String> = filtered_zones(&rt.known_zones, rt.zone.value());
            match key.code {
                KeyCode::Esc => rt.zone_dropdown_open = false,
                KeyCode::Tab => {
                    rt.zone_dropdown_open = false;
                    rt.next_field();
                }
                KeyCode::BackTab => {
                    rt.zone_dropdown_open = false;
                    rt.prev_field();
                }
                KeyCode::Up => {
                    if rt.zone_idx > 0 {
                        rt.zone_idx -= 1;
                    }
                }
                KeyCode::Down => {
                    if rt.zone_idx + 1 < matches.len() {
                        rt.zone_idx += 1;
                    }
                }
                KeyCode::Enter => {
                    if let Some(d) = matches.get(rt.zone_idx) {
                        rt.zone = Input::new(d);
                        committed_zone = true;
                    }
                    rt.zone_dropdown_open = false;
                }
                KeyCode::Char(c) => {
                    rt.zone.insert(c);
                    rt.zone_idx = 0;
                }
                KeyCode::Backspace => {
                    rt.zone.backspace();
                    rt.zone_idx = 0;
                }
                KeyCode::Delete => rt.zone.delete(),
                KeyCode::Left => rt.zone.left(),
                KeyCode::Right => rt.zone.right(),
                KeyCode::Home => rt.zone.home(),
                KeyCode::End => rt.zone.end_of_line(),
                _ => {}
            }
        }
        if committed_zone {
            self.trigger_fetch_records();
        }
    }

    fn trigger_fetch_records(&mut self) {
        let label = self.records_tab.account_input.value().trim().to_string();
        let Some(account) = self.account_by_label(&label) else {
            self.modal = Some(("Error".into(), "Select a valid account (use the dropdown)".into()));
            return;
        };
        if self.records_tab.known_zones.is_empty() {
            self.spawn_zones_refresh(account.clone());
        }
        let zone_name = self.records_tab.zone.value().trim().to_string();
        let zone_id = self.records_tab.zone_id_for(&zone_name);
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = if zone_name.is_empty() {
                api::fetch_account_records(&account)
            } else if let Some(zone_id) = zone_id {
                match api::get_zone_records(&account, &zone_id, &zone_name) {
                    Ok(records) => Ok((records, 1, 0)),
                    Err(e) => Err(e.message),
                }
            } else {
                Err(format!("Unknown zone '{zone_name}' — pick one from the dropdown"))
            };
            let _ = tx.send(CfMsg::RecordsResult(result));
        });
    }

    /// Auto-loads every account's records the first time the Records tab
    /// is opened with nothing loaded yet, so zones just show up instead of
    /// requiring a manual "Fetch All Accounts" click every time. Only fires
    /// once per session (until `records_tab.rows` is populated) and stays
    /// silent — no accounts configured yet is a normal state while setting
    /// the tool up, not something worth an error popup on every tab switch.
    fn trigger_fetch_all_if_empty(&mut self) {
        if !self.cfg.accounts.is_empty() && self.records_tab.rows.is_empty() {
            self.trigger_fetch_all_accounts();
        }
    }

    fn trigger_fetch_all_accounts(&mut self) {
        if self.cfg.accounts.is_empty() {
            self.modal = Some(("Error".into(), "Add at least one account first (F1 Accounts)".into()));
            return;
        }
        let accounts: Vec<AccountWithSecret> = self.cfg.accounts.iter().filter_map(|a| self.cfg.with_secret(&a.id)).collect();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let (records, zones, skipped, errors) = api::fetch_all_accounts(accounts);
            for (label, msg) in &errors {
                let _ = tx.send(CfMsg::Log(false, format!("[{label}] {}", one_line(msg))));
            }
            let _ = tx.send(CfMsg::RecordsResult(Ok((records, zones, skipped))));
        });
    }

    fn trigger_open_add_modal(&mut self) {
        let label = self.records_tab.account_input.value().trim().to_string();
        let Some(account) = self.account_by_label(&label) else {
            self.modal = Some(("Error".into(), "Select a valid account first (Account field above, use the dropdown)".into()));
            return;
        };
        if self.records_tab.known_zones.is_empty() {
            self.spawn_zones_refresh(account);
        }
        let zone = self.records_tab.zone.value().trim().to_string();
        self.records_tab.modal = Some(RecordModal::Add(Box::new(AddModal::new(zone))));
    }

    // ── Record modals: dispatch ──────────────────────────────────────
    fn handle_record_modal_key(&mut self, key: KeyEvent) {
        match self.records_tab.modal.take() {
            Some(RecordModal::Add(mut m)) => {
                let close = self.handle_add_modal_key(&mut m, key);
                if !close {
                    self.records_tab.modal = Some(RecordModal::Add(m));
                }
            }
            Some(RecordModal::Edit(mut m)) => {
                let close = self.handle_edit_modal_key(&mut m, key);
                if !close {
                    self.records_tab.modal = Some(RecordModal::Edit(m));
                }
            }
            None => {}
        }
    }

    fn handle_add_modal_zone_dropdown_key(&mut self, m: &mut AddModal, key: KeyEvent) {
        let matches: Vec<String> = filtered_zones(&self.records_tab.known_zones, m.zone.value());
        match key.code {
            KeyCode::Esc => m.zone_dropdown_open = false,
            KeyCode::Tab => {
                m.zone_dropdown_open = false;
                m.next_field();
            }
            KeyCode::BackTab => {
                m.zone_dropdown_open = false;
                m.prev_field();
            }
            KeyCode::Up => {
                if m.zone_idx > 0 {
                    m.zone_idx -= 1;
                }
            }
            KeyCode::Down => {
                if m.zone_idx + 1 < matches.len() {
                    m.zone_idx += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(d) = matches.get(m.zone_idx) {
                    m.zone = Input::new(d);
                }
                m.zone_dropdown_open = false;
            }
            KeyCode::Char(c) => {
                m.zone.insert(c);
                m.zone_idx = 0;
            }
            KeyCode::Backspace => {
                m.zone.backspace();
                m.zone_idx = 0;
            }
            KeyCode::Delete => m.zone.delete(),
            KeyCode::Left => m.zone.left(),
            KeyCode::Right => m.zone.right(),
            KeyCode::Home => m.zone.home(),
            KeyCode::End => m.zone.end_of_line(),
            _ => {}
        }
    }

    fn handle_add_modal_key(&mut self, m: &mut AddModal, key: KeyEvent) -> bool {
        if m.zone_dropdown_open {
            self.handle_add_modal_zone_dropdown_key(m, key);
            return false;
        }

        let is_discard_attempt = key.code == KeyCode::Esc || (key.code == KeyCode::Enter && m.field == AddField::BtnCancel);
        if !is_discard_attempt {
            m.confirm_discard = false;
        }

        if m.field == AddField::PendingList && !m.pending.is_empty() {
            match key.code {
                KeyCode::Up => {
                    if m.pending_idx > 0 {
                        m.pending_idx -= 1;
                    }
                    return false;
                }
                KeyCode::Down => {
                    if m.pending_idx + 1 < m.pending.len() {
                        m.pending_idx += 1;
                    }
                    return false;
                }
                KeyCode::Delete => {
                    m.pending.remove(m.pending_idx);
                    if m.pending_idx >= m.pending.len() {
                        m.pending_idx = m.pending.len().saturating_sub(1);
                    }
                    return false;
                }
                _ => {}
            }
        }

        if m.field == AddField::Type {
            match key.code {
                KeyCode::Left => {
                    m.type_idx = (m.type_idx + RECORD_TYPES.len() - 1) % RECORD_TYPES.len();
                    return false;
                }
                KeyCode::Right => {
                    m.type_idx = (m.type_idx + 1) % RECORD_TYPES.len();
                    return false;
                }
                _ => {}
            }
        }
        if m.field == AddField::Tag {
            match key.code {
                KeyCode::Left => {
                    m.tag_idx = (m.tag_idx + CAA_TAGS.len() - 1) % CAA_TAGS.len();
                    return false;
                }
                KeyCode::Right => {
                    m.tag_idx = (m.tag_idx + 1) % CAA_TAGS.len();
                    return false;
                }
                _ => {}
            }
        }
        if m.field == AddField::Proxied {
            match key.code {
                KeyCode::Left | KeyCode::Right => {
                    m.proxied = !m.proxied;
                    return false;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {
                if !m.pending.is_empty() && !m.confirm_discard {
                    m.confirm_discard = true;
                    return false;
                }
                return true;
            }
            KeyCode::Tab => m.next_field(),
            KeyCode::BackTab => m.prev_field(),
            KeyCode::Up => m.prev_field(),
            KeyCode::Down => m.next_field(),
            KeyCode::Enter => match m.field {
                AddField::Zone => {
                    if !self.records_tab.known_zones.is_empty() {
                        m.zone_idx = 0;
                        m.zone_dropdown_open = true;
                    } else {
                        m.next_field();
                    }
                }
                AddField::Type => m.type_idx = (m.type_idx + 1) % RECORD_TYPES.len(),
                AddField::Tag => m.tag_idx = (m.tag_idx + 1) % CAA_TAGS.len(),
                AddField::Proxied => m.proxied = !m.proxied,
                AddField::BtnStage => {
                    match parse_modal_record(m.type_idx, &m.name, &m.priority, &m.weight, &m.port, &m.flags, m.tag_idx, m.proxied, &m.value, &m.ttl) {
                        Ok(input) => {
                            m.pending.push(input);
                            m.clear_inputs();
                        }
                        Err(e) => self.modal = Some(("Error".into(), e)),
                    }
                }
                AddField::BtnSaveAll => {
                    let zone_name = m.zone.value().trim().to_string();
                    if zone_name.is_empty() {
                        self.modal = Some(("Error".into(), "Pick or type a zone first".into()));
                        return false;
                    }
                    if m.pending.is_empty() {
                        self.modal = Some(("Error".into(), "Stage at least one record first (\"+ Add to list\")".into()));
                        return false;
                    }
                    let label = self.records_tab.account_input.value().trim().to_string();
                    let Some(account) = self.account_by_label(&label) else {
                        self.modal = Some(("Error".into(), "Select a valid account".into()));
                        return false;
                    };
                    let Some(zone_id) = self.records_tab.zone_id_for(&zone_name) else {
                        self.modal = Some(("Error".into(), format!("Unknown zone '{zone_name}' — pick one from the dropdown")));
                        return false;
                    };
                    let batch = m.pending.clone();
                    let tx = self.tx.clone();
                    let account_for_refresh = account.clone();
                    let zone_id_for_refresh = zone_id.clone();
                    let zone_name_for_refresh = zone_name.clone();
                    thread::spawn(move || {
                        let total = batch.len();
                        let mut ok_count = 0usize;
                        for input in &batch {
                            match api::add_record(&account, &zone_id, &zone_name, input) {
                                Ok(()) => ok_count += 1,
                                Err(e) => {
                                    let _ = tx.send(CfMsg::Log(false, format!("[{zone_name}] failed to add {} {}: {}", input.type_, input.subdomain, one_line(&e))));
                                }
                            }
                        }
                        let _ = tx.send(CfMsg::Log(ok_count == total, format!("[{zone_name}] added {ok_count}/{total} record(s)")));
                        let result = match api::get_zone_records(&account_for_refresh, &zone_id_for_refresh, &zone_name_for_refresh) {
                            Ok(records) => Ok((records, 1, 0)),
                            Err(e) => Err(e.message),
                        };
                        let _ = tx.send(CfMsg::RecordsResult(result));
                    });
                    return true;
                }
                AddField::BtnCancel => {
                    if !m.pending.is_empty() && !m.confirm_discard {
                        m.confirm_discard = true;
                        return false;
                    }
                    return true;
                }
                _ => m.next_field(),
            },
            KeyCode::Char(c) => match m.field {
                AddField::Zone => {
                    m.zone.insert(c);
                    if !self.records_tab.known_zones.is_empty() {
                        m.zone_dropdown_open = true;
                        m.zone_idx = 0;
                    }
                }
                AddField::Name => m.name.insert(c),
                AddField::Priority => m.priority.insert(c),
                AddField::Weight => m.weight.insert(c),
                AddField::Port => m.port.insert(c),
                AddField::Flags => m.flags.insert(c),
                AddField::Value => m.value.insert(c),
                AddField::Ttl => m.ttl.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match m.field {
                AddField::Zone => m.zone.backspace(),
                AddField::Name => m.name.backspace(),
                AddField::Priority => m.priority.backspace(),
                AddField::Weight => m.weight.backspace(),
                AddField::Port => m.port.backspace(),
                AddField::Flags => m.flags.backspace(),
                AddField::Value => m.value.backspace(),
                AddField::Ttl => m.ttl.backspace(),
                _ => {}
            },
            KeyCode::Delete => match m.field {
                AddField::Zone => m.zone.delete(),
                AddField::Name => m.name.delete(),
                AddField::Priority => m.priority.delete(),
                AddField::Weight => m.weight.delete(),
                AddField::Port => m.port.delete(),
                AddField::Flags => m.flags.delete(),
                AddField::Value => m.value.delete(),
                AddField::Ttl => m.ttl.delete(),
                _ => {}
            },
            KeyCode::Left => match m.field {
                AddField::Zone => m.zone.left(),
                AddField::Name => m.name.left(),
                AddField::Priority => m.priority.left(),
                AddField::Weight => m.weight.left(),
                AddField::Port => m.port.left(),
                AddField::Flags => m.flags.left(),
                AddField::Value => m.value.left(),
                AddField::Ttl => m.ttl.left(),
                _ => {}
            },
            KeyCode::Right => match m.field {
                AddField::Zone => m.zone.right(),
                AddField::Name => m.name.right(),
                AddField::Priority => m.priority.right(),
                AddField::Weight => m.weight.right(),
                AddField::Port => m.port.right(),
                AddField::Flags => m.flags.right(),
                AddField::Value => m.value.right(),
                AddField::Ttl => m.ttl.right(),
                _ => {}
            },
            KeyCode::Home => match m.field {
                AddField::Zone => m.zone.home(),
                AddField::Name => m.name.home(),
                AddField::Priority => m.priority.home(),
                AddField::Weight => m.weight.home(),
                AddField::Port => m.port.home(),
                AddField::Flags => m.flags.home(),
                AddField::Value => m.value.home(),
                AddField::Ttl => m.ttl.home(),
                _ => {}
            },
            KeyCode::End => match m.field {
                AddField::Zone => m.zone.end_of_line(),
                AddField::Name => m.name.end_of_line(),
                AddField::Priority => m.priority.end_of_line(),
                AddField::Weight => m.weight.end_of_line(),
                AddField::Port => m.port.end_of_line(),
                AddField::Flags => m.flags.end_of_line(),
                AddField::Value => m.value.end_of_line(),
                AddField::Ttl => m.ttl.end_of_line(),
                _ => {}
            },
            _ => {}
        }
        false
    }

    fn handle_edit_modal_key(&mut self, m: &mut EditModal, key: KeyEvent) -> bool {
        if m.field == EditField::Type {
            match key.code {
                KeyCode::Left => {
                    m.type_idx = (m.type_idx + RECORD_TYPES.len() - 1) % RECORD_TYPES.len();
                    return false;
                }
                KeyCode::Right => {
                    m.type_idx = (m.type_idx + 1) % RECORD_TYPES.len();
                    return false;
                }
                _ => {}
            }
        }
        if m.field == EditField::Tag {
            match key.code {
                KeyCode::Left => {
                    m.tag_idx = (m.tag_idx + CAA_TAGS.len() - 1) % CAA_TAGS.len();
                    return false;
                }
                KeyCode::Right => {
                    m.tag_idx = (m.tag_idx + 1) % CAA_TAGS.len();
                    return false;
                }
                _ => {}
            }
        }
        if m.field == EditField::Proxied {
            match key.code {
                KeyCode::Left | KeyCode::Right => {
                    m.proxied = !m.proxied;
                    return false;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => return true,
            KeyCode::Tab => m.next_field(),
            KeyCode::BackTab => m.prev_field(),
            KeyCode::Up => m.prev_field(),
            KeyCode::Down => m.next_field(),
            KeyCode::Enter => match m.field {
                EditField::Type => m.type_idx = (m.type_idx + 1) % RECORD_TYPES.len(),
                EditField::Tag => m.tag_idx = (m.tag_idx + 1) % CAA_TAGS.len(),
                EditField::Proxied => m.proxied = !m.proxied,
                EditField::BtnUpdate => match parse_modal_record(m.type_idx, &m.name, &m.priority, &m.weight, &m.port, &m.flags, m.tag_idx, m.proxied, &m.value, &m.ttl) {
                    Ok(input) => {
                        let label = self.records_tab.account_input.value().trim().to_string();
                        let Some(account) = self.account_by_label(&label) else {
                            self.modal = Some(("Error".into(), "Select a valid account".into()));
                            return false;
                        };
                        let old = m.original.clone();
                        let zone_id = old.zone_id.clone();
                        let zone_name = old.domain.clone();
                        let record_id = old.id.clone();
                        let tx = self.tx.clone();
                        let account_for_refresh = account.clone();
                        let zone_id_for_refresh = zone_id.clone();
                        let refresh_zone_name = self.records_tab.zone.value().trim().to_string();
                        let zone_name_for_fallback = zone_name.clone();
                        thread::spawn(move || {
                            let msg = match api::update_record(&account, &zone_id, &zone_name, &record_id, &input) {
                                Ok(()) => (true, format!("[{zone_name}] updated {} {}", input.type_, input.subdomain)),
                                Err(e) => (false, format!("[{zone_name}] failed to update {}: {}", input.type_, one_line(&e))),
                            };
                            let _ = tx.send(CfMsg::Log(msg.0, msg.1));
                            let fetch_zone_name = if refresh_zone_name.is_empty() { zone_name_for_fallback } else { refresh_zone_name };
                            let result = match api::get_zone_records(&account_for_refresh, &zone_id_for_refresh, &fetch_zone_name) {
                                Ok(records) => Ok((records, 1, 0)),
                                Err(e) => Err(e.message),
                            };
                            let _ = tx.send(CfMsg::RecordsResult(result));
                        });
                        return true;
                    }
                    Err(e) => {
                        self.modal = Some(("Error".into(), e));
                        return false;
                    }
                },
                EditField::BtnDelete => {
                    let label = self.records_tab.account_input.value().trim().to_string();
                    let Some(account) = self.account_by_label(&label) else {
                        self.modal = Some(("Error".into(), "Select a valid account".into()));
                        return false;
                    };
                    let old = m.original.clone();
                    let zone_id = old.zone_id.clone();
                    let zone_name = old.domain.clone();
                    let record_id = old.id.clone();
                    let tx = self.tx.clone();
                    let account_for_refresh = account.clone();
                    let zone_id_for_refresh = zone_id.clone();
                    let refresh_zone_name = self.records_tab.zone.value().trim().to_string();
                    let zone_name_for_fallback = zone_name.clone();
                    thread::spawn(move || {
                        let msg = match api::delete_record(&account, &zone_id, &zone_name, &record_id) {
                            Ok(()) => (true, format!("[{zone_name}] deleted {} {}", old.type_, old.subdomain)),
                            Err(e) => (false, format!("[{zone_name}] failed to delete {}: {}", old.type_, one_line(&e))),
                        };
                        let _ = tx.send(CfMsg::Log(msg.0, msg.1));
                        let fetch_zone_name = if refresh_zone_name.is_empty() { zone_name_for_fallback } else { refresh_zone_name };
                        let result = match api::get_zone_records(&account_for_refresh, &zone_id_for_refresh, &fetch_zone_name) {
                            Ok(records) => Ok((records, 1, 0)),
                            Err(e) => Err(e.message),
                        };
                        let _ = tx.send(CfMsg::RecordsResult(result));
                    });
                    return true;
                }
                EditField::BtnCancel => return true,
                _ => m.next_field(),
            },
            KeyCode::Char(c) => match m.field {
                EditField::Name => m.name.insert(c),
                EditField::Priority => m.priority.insert(c),
                EditField::Weight => m.weight.insert(c),
                EditField::Port => m.port.insert(c),
                EditField::Flags => m.flags.insert(c),
                EditField::Value => m.value.insert(c),
                EditField::Ttl => m.ttl.insert(c),
                _ => {}
            },
            KeyCode::Backspace => match m.field {
                EditField::Name => m.name.backspace(),
                EditField::Priority => m.priority.backspace(),
                EditField::Weight => m.weight.backspace(),
                EditField::Port => m.port.backspace(),
                EditField::Flags => m.flags.backspace(),
                EditField::Value => m.value.backspace(),
                EditField::Ttl => m.ttl.backspace(),
                _ => {}
            },
            KeyCode::Delete => match m.field {
                EditField::Name => m.name.delete(),
                EditField::Priority => m.priority.delete(),
                EditField::Weight => m.weight.delete(),
                EditField::Port => m.port.delete(),
                EditField::Flags => m.flags.delete(),
                EditField::Value => m.value.delete(),
                EditField::Ttl => m.ttl.delete(),
                _ => {}
            },
            KeyCode::Left => match m.field {
                EditField::Name => m.name.left(),
                EditField::Priority => m.priority.left(),
                EditField::Weight => m.weight.left(),
                EditField::Port => m.port.left(),
                EditField::Flags => m.flags.left(),
                EditField::Value => m.value.left(),
                EditField::Ttl => m.ttl.left(),
                _ => {}
            },
            KeyCode::Right => match m.field {
                EditField::Name => m.name.right(),
                EditField::Priority => m.priority.right(),
                EditField::Weight => m.weight.right(),
                EditField::Port => m.port.right(),
                EditField::Flags => m.flags.right(),
                EditField::Value => m.value.right(),
                EditField::Ttl => m.ttl.right(),
                _ => {}
            },
            KeyCode::Home => match m.field {
                EditField::Name => m.name.home(),
                EditField::Priority => m.priority.home(),
                EditField::Weight => m.weight.home(),
                EditField::Port => m.port.home(),
                EditField::Flags => m.flags.home(),
                EditField::Value => m.value.home(),
                EditField::Ttl => m.ttl.home(),
                _ => {}
            },
            KeyCode::End => match m.field {
                EditField::Name => m.name.end_of_line(),
                EditField::Priority => m.priority.end_of_line(),
                EditField::Weight => m.weight.end_of_line(),
                EditField::Port => m.port.end_of_line(),
                EditField::Flags => m.flags.end_of_line(),
                EditField::Value => m.value.end_of_line(),
                EditField::Ttl => m.ttl.end_of_line(),
                _ => {}
            },
            _ => {}
        }
        false
    }

    // ── Drawing ───────────────────────────────────────────────────────
    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(0)]).split(area);

        let tab_bar = Line::from(vec![
            tab_span("F1 Accounts", self.tab == Tab::Accounts),
            Span::styled("  ", Style::default().bg(BG)),
            tab_span("F2 Records", self.tab == Tab::Records),
            Span::styled("  ", Style::default().bg(BG)),
            Span::styled("Esc back  Ctrl+C quit", Style::default().fg(FG2).bg(BG)),
        ]);
        f.render_widget(Paragraph::new(tab_bar).style(Style::default().bg(BG)), chunks[0]);

        match self.tab {
            Tab::Accounts => self.draw_accounts(f, chunks[1]),
            Tab::Records => self.draw_records(f, chunks[1]),
        }

        if let Tab::Records = self.tab {
            match &self.records_tab.modal {
                Some(RecordModal::Add(m)) => self.draw_add_modal(f, m, area),
                Some(RecordModal::Edit(m)) => self.draw_edit_modal(f, m, area),
                None => {}
            }
        }

        if let Some((title, msg)) = &self.modal {
            draw_modal(f, title, msg, area);
        }
    }

    fn draw_accounts(&self, f: &mut Frame, area: Rect) {
        let at = &self.accounts_tab;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(12), Constraint::Length(6)])
            .split(area);

        let header = Row::new(vec![
            Cell::from(Span::styled("Label", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("API Token", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
        ])
        .style(Style::default().bg(BG2));

        let rows: Vec<Row> = self
            .cfg
            .accounts
            .iter()
            .map(|a| Row::new(vec![Cell::from(a.label.clone()), Cell::from("\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022} (encrypted)")]))
            .collect();

        let table = Table::new(rows, [Constraint::Length(24), Constraint::Min(0)])
            .header(header)
            .block(theme_block(" Accounts "))
            .row_highlight_style(if at.field == AccField::Table { focused() } else { normal() })
            .highlight_symbol(" \u{25B6} ")
            .style(Style::default().fg(FG).bg(BG));

        let mut tstate = TableState::default();
        if !self.cfg.accounts.is_empty() {
            tstate.select(Some(at.table_idx.min(self.cfg.accounts.len() - 1)));
        }
        f.render_stateful_widget(table, chunks[0], &mut tstate);

        let form_block = theme_block(" Add / Edit Account ");
        let form_inner = form_block.inner(chunks[1]);
        f.render_widget(form_block, chunks[1]);

        let rows2 = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // [0] Label
                Constraint::Length(1), // [1] spacer
                Constraint::Length(1), // [2] API Token
                Constraint::Length(1), // [3] spacer
                Constraint::Length(1), // [4] hint
                Constraint::Length(1), // [5] spacer
                Constraint::Length(1), // [6] buttons
                Constraint::Length(1), // [7] spacer
                Constraint::Length(1), // [8] nav hint
                Constraint::Min(0),
            ])
            .split(form_inner);
        let fw = rows2[0].width.saturating_sub(14) as usize;

        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Label:        ", lbl()), input_span(&at.label, at.field == AccField::Label, false, fw)])),
            rows2[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("API Token:    ", lbl()), input_span(&at.api_token, at.field == AccField::ApiToken, true, fw)])),
            rows2[2],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                if at.selected.is_some() { "editing existing account — re-enter the token to update it" } else { "creating a new account — needs a scoped API Token with Zone:Read + DNS:Edit" },
                lbl(),
            ))),
            rows2[4],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                btn_span("Save", at.field == AccField::BtnSave),
                Span::raw("  "),
                btn_span("New", at.field == AccField::BtnNew),
                Span::raw("  "),
                btn_span("Delete", at.field == AccField::BtnDelete),
                Span::raw("  "),
                btn_span("Test Token", at.field == AccField::BtnTest),
            ])),
            rows2[6],
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled("\u{2191}\u{2193} select row  Enter load row  Tab navigate  Esc back", lbl()))),
            rows2[8],
        );

        draw_history(f, &self.history, chunks[2], self.history_scroll);
    }

    fn draw_records(&self, f: &mut Frame, area: Rect) {
        let rt = &self.records_tab;
        let visible = self.visible_rows();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(7), Constraint::Length(7), Constraint::Min(6), Constraint::Length(6), Constraint::Length(7)])
            .split(area);

        // ── Fetch box ──
        let top_block = theme_block(" Fetch ");
        let top_inner = top_block.inner(chunks[0]);
        f.render_widget(top_block, chunks[0]);
        let top_rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
            .split(top_inner);
        let fw = top_rows[0].width.saturating_sub(12) as usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Account:    ", lbl()), input_span(&rt.account_input, rt.field == RecField::Account, false, fw)])),
            top_rows[0],
        );
        let fw2 = top_rows[2].width.saturating_sub(12 + 2 + 16) as usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Zone:       ", lbl()),
                input_span(&rt.zone, rt.field == RecField::Zone, false, fw2),
                Span::raw("  "),
                btn_span("Fetch", rt.field == RecField::BtnFetch),
            ])),
            top_rows[2],
        );

        // ── Search / filter box (global search lives here) ──
        let search_block = theme_block(" Search (across everything currently loaded) ");
        let search_inner = search_block.inner(chunks[1]);
        f.render_widget(search_block, chunks[1]);
        let search_rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
            .split(search_inner);
        let search_fw = search_rows[0].width.saturating_sub(9) as usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Search: ", lbl()), input_span(&rt.search, rt.field == RecField::Search, false, search_fw)])),
            search_rows[0],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Type: ", lbl()),
                btn_span(TYPE_FILTERS[rt.type_filter_idx], rt.field == RecField::TypeFilter),
                Span::styled("  (\u{2190}/\u{2192})   ", lbl()),
                btn_span("Fetch All Accounts", rt.field == RecField::BtnFetchAll),
                Span::styled("   Account field above also filters", lbl()),
            ])),
            search_rows[2],
        );

        // ── Records table ──
        let header = Row::new(vec![
            Cell::from(Span::styled("Zone", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Type", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Name", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Value", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("TTL", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Proxy", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
            Cell::from(Span::styled("Account", Style::default().fg(TITLE).add_modifier(Modifier::BOLD))),
        ])
        .style(Style::default().bg(BG2));

        let table_rows: Vec<Row> = visible
            .iter()
            .map(|r| {
                let proxy_cell = if is_proxiable(&r.type_) {
                    if r.proxied {
                        Cell::from(Span::styled("\u{2601} on", Style::default().fg(ACCENT)))
                    } else {
                        Cell::from("off")
                    }
                } else {
                    Cell::from("\u{2014}")
                };
                Row::new(vec![
                    Cell::from(r.domain.clone()),
                    Cell::from(r.type_.clone()),
                    Cell::from(r.subdomain.clone()),
                    Cell::from(r.value.clone()),
                    Cell::from(fmt_ttl(r.ttl)),
                    proxy_cell,
                    Cell::from(r.account_label.clone()),
                ])
            })
            .collect();

        let records_title = format!(" Records ({} of {}) ", visible.len(), rt.rows.len());
        let table = Table::new(
            table_rows,
            [
                Constraint::Length(18),
                Constraint::Length(6),
                Constraint::Length(12),
                Constraint::Min(16),
                Constraint::Length(6),
                Constraint::Length(7),
                Constraint::Length(12),
            ],
        )
        .header(header)
        .block(theme_block(&records_title))
        .row_highlight_style(if rt.field == RecField::Table { focused() } else { normal() })
        .highlight_symbol(" \u{25B6} ")
        .style(Style::default().fg(FG).bg(BG));

        let mut tstate = TableState::default();
        if !visible.is_empty() {
            tstate.select(Some(rt.selected_row.min(visible.len() - 1)));
        }
        f.render_stateful_widget(table, chunks[2], &mut tstate);

        // ── Actions box ──
        let action_block = theme_block(" Actions ");
        let action_inner = action_block.inner(chunks[3]);
        f.render_widget(action_block, chunks[3]);
        let action_rows = Layout::default().direction(Direction::Vertical).margin(1).constraints([Constraint::Length(1), Constraint::Length(1)]).split(action_inner);
        f.render_widget(Paragraph::new(Line::from(vec![btn_span("Add Record(s)", rt.field == RecField::BtnAddRecords)])), action_rows[0]);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Enter on a table row opens Edit/Delete  \u{2022}  y copies its Value  \u{2022}  Add lets you pick the zone inside",
                lbl(),
            ))),
            action_rows[1],
        );

        draw_history(f, &self.history, chunks[4], self.history_scroll);

        if rt.account_dropdown_open {
            render_dropdown(f, &filtered_accounts(&self.cfg, rt.account_input.value()), rt.account_idx, top_rows[0], 12, area);
        }
        if rt.zone_dropdown_open {
            render_dropdown(f, &filtered_zones(&rt.known_zones, rt.zone.value()), rt.zone_idx, top_rows[2], 12, area);
        }
    }

    fn draw_add_modal(&self, f: &mut Frame, m: &AddModal, area: Rect) {
        let width = 92u16.min(area.width.saturating_sub(4));
        let height = 23u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);

        f.render_widget(Clear, modal_area);
        let block = Block::default()
            .title(Span::styled(" Add DNS Record(s) ", Style::default().fg(TITLE)))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(BG2));
        let inner = block.inner(modal_area);
        f.render_widget(block, modal_area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // [0]  Zone
                Constraint::Length(1), // [1]  spacer
                Constraint::Length(1), // [2]  Type / Name / TTL (/ Proxied)
                Constraint::Length(1), // [3]  spacer
                Constraint::Length(1), // [4]  type-specific fields (MX/SRV/CAA only)
                Constraint::Length(1), // [5]  spacer
                Constraint::Length(1), // [6]  Value
                Constraint::Length(1), // [7]  spacer
                Constraint::Length(1), // [8]  stage button
                Constraint::Length(1), // [9]  spacer
                Constraint::Length(1), // [10] "Pending" label
                Constraint::Min(3),    // [11] pending list
                Constraint::Length(1), // [12] spacer
                Constraint::Length(1), // [13] Save All / Cancel
                Constraint::Length(1), // [14] hint
            ])
            .split(inner);

        let zone_fw = rows[0].width.saturating_sub(7) as usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Zone: ", lbl()), input_span(&m.zone, m.field == AddField::Zone, false, zone_fw)])),
            rows[0],
        );

        let mut type_line = vec![
            Span::styled("Type: ", lbl()),
            btn_span(m.current_type(), m.field == AddField::Type),
            Span::styled(" (\u{2190}/\u{2192})   ", lbl()),
            Span::styled("Name: ", lbl()),
            input_span(&m.name, m.field == AddField::Name, false, 20),
            Span::raw("  "),
            Span::styled("TTL: ", lbl()),
            input_span(&m.ttl, m.field == AddField::Ttl, false, 8),
        ];
        if is_proxiable(m.current_type()) {
            type_line.push(Span::raw("  "));
            type_line.push(Span::styled("Proxied: ", lbl()));
            type_line.push(btn_span(if m.proxied { "Yes" } else { "No" }, m.field == AddField::Proxied));
        }
        f.render_widget(Paragraph::new(Line::from(type_line)), rows[2]);

        draw_type_specific_add_row(f, m, rows[4]);
        let value_fw = rows[6].width.saturating_sub(7) as usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Value: ", lbl()), input_span(&m.value, m.field == AddField::Value, false, value_fw)])),
            rows[6],
        );
        f.render_widget(Paragraph::new(Line::from(vec![btn_span("+ Add to list", m.field == AddField::BtnStage)])), rows[8]);
        f.render_widget(Paragraph::new(Line::from(Span::styled(format!("Pending ({}):", m.pending.len()), lbl()))), rows[10]);

        if m.pending.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled("  (none yet — pick a zone, fill the form above, and \"+ Add to list\")", lbl()))),
                rows[11],
            );
        } else {
            let lines: Vec<Line> = m
                .pending
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let selected = m.field == AddField::PendingList && i == m.pending_idx;
                    let prefix = if selected { " \u{25B6} " } else { "   " };
                    let style = if selected { focused() } else { normal() };
                    let proxy_flag = if is_proxiable(&r.type_) { if r.proxied { " \u{2601}" } else { "" } } else { "" };
                    Line::from(Span::styled(format!("{prefix}{:<6} {:<16} {:<28} {}{}", r.type_, r.subdomain, r.value, fmt_ttl(r.ttl), proxy_flag), style))
                })
                .collect();
            f.render_widget(Paragraph::new(Text::from(lines)), rows[11]);
        }

        f.render_widget(
            Paragraph::new(Line::from(vec![btn_span("Save All", m.field == AddField::BtnSaveAll), Span::raw("  "), btn_span("Cancel", m.field == AddField::BtnCancel)])),
            rows[13],
        );
        if m.confirm_discard {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("\u{26a0} {} staged record(s) NOT saved yet \u{2014} Esc/Cancel again to discard, or Tab to \"Save All\"", m.pending.len()),
                    Style::default().fg(RED).add_modifier(Modifier::BOLD),
                ))),
                rows[14],
            );
        } else {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "\"+ Add to list\" stages only, \"Save All\" creates them  \u{2022}  \u{2191}\u{2193}/Del on list to remove  \u{2022}  Esc cancel",
                    lbl(),
                ))),
                rows[14],
            );
        }

        if m.zone_dropdown_open {
            render_dropdown(f, &filtered_zones(&self.records_tab.known_zones, m.zone.value()), m.zone_idx, rows[0], 6, area);
        } else if self.records_tab.known_zones.is_empty() && m.field == AddField::Zone {
            let hint_area = Rect::new(rows[0].x, rows[0].y.saturating_add(1), rows[0].width.min(70), 1);
            f.render_widget(
                Paragraph::new(Span::styled("no zones loaded for this account yet — type one, or wait a moment and retry", Style::default().fg(YELLOW))),
                hint_area,
            );
        }
    }

    fn draw_edit_modal(&self, f: &mut Frame, m: &EditModal, area: Rect) {
        let width = 92u16.min(area.width.saturating_sub(4));
        let height = 15u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);

        f.render_widget(Clear, modal_area);
        let block = Block::default()
            .title(Span::styled(format!(" Edit / Delete Record — {} ", m.original.domain), Style::default().fg(TITLE)))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(BG2));
        let inner = block.inner(modal_area);
        f.render_widget(block, modal_area);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // 0 Type / Name / TTL (/ Proxied)
                Constraint::Length(1), // 1 spacer
                Constraint::Length(1), // 2 type-specific fields (MX/SRV/CAA only)
                Constraint::Length(1), // 3 spacer
                Constraint::Length(1), // 4 Value
                Constraint::Length(1), // 5 spacer
                Constraint::Length(1), // 6 buttons
                Constraint::Length(1), // 7 spacer
                Constraint::Length(1), // 8 hint
                Constraint::Min(0),
            ])
            .split(inner);

        let mut type_line = vec![
            Span::styled("Type: ", lbl()),
            btn_span(m.current_type(), m.field == EditField::Type),
            Span::styled(" (\u{2190}/\u{2192})   ", lbl()),
            Span::styled("Name: ", lbl()),
            input_span(&m.name, m.field == EditField::Name, false, 20),
            Span::raw("  "),
            Span::styled("TTL: ", lbl()),
            input_span(&m.ttl, m.field == EditField::Ttl, false, 8),
        ];
        if is_proxiable(m.current_type()) {
            type_line.push(Span::raw("  "));
            type_line.push(Span::styled("Proxied: ", lbl()));
            type_line.push(btn_span(if m.proxied { "Yes" } else { "No" }, m.field == EditField::Proxied));
        }
        f.render_widget(Paragraph::new(Line::from(type_line)), rows[0]);

        draw_type_specific_edit_row(f, m, rows[2]);
        let value_fw = rows[4].width.saturating_sub(7) as usize;
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("Value: ", lbl()), input_span(&m.value, m.field == EditField::Value, false, value_fw)])),
            rows[4],
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                btn_span("Update", m.field == EditField::BtnUpdate),
                Span::raw("  "),
                btn_span("Delete", m.field == EditField::BtnDelete),
                Span::raw("  "),
                btn_span("Cancel", m.field == EditField::BtnCancel),
            ])),
            rows[6],
        );
        f.render_widget(Paragraph::new(Line::from(Span::styled("Tab navigate  \u{2022}  Enter activate  \u{2022}  Esc cancel", lbl()))), rows[8]);
    }
}

/// Renders the MX/SRV/CAA-only extra field line for the Add modal; a no-op
/// (blank row) for every other type.
fn draw_type_specific_add_row(f: &mut Frame, m: &AddModal, area: Rect) {
    let line = match m.current_type() {
        "MX" => Some(Line::from(vec![Span::styled("Priority: ", lbl()), input_span(&m.priority, m.field == AddField::Priority, false, 8), Span::styled("  (lower number = higher priority)", lbl())])),
        "SRV" => Some(Line::from(vec![
            Span::styled("Priority: ", lbl()),
            input_span(&m.priority, m.field == AddField::Priority, false, 6),
            Span::raw("  "),
            Span::styled("Weight: ", lbl()),
            input_span(&m.weight, m.field == AddField::Weight, false, 6),
            Span::raw("  "),
            Span::styled("Port: ", lbl()),
            input_span(&m.port, m.field == AddField::Port, false, 6),
            Span::styled("  (Name like _service._proto)", lbl()),
        ])),
        "CAA" => Some(Line::from(vec![
            Span::styled("Flags: ", lbl()),
            input_span(&m.flags, m.field == AddField::Flags, false, 4),
            Span::styled("  (0-255)   ", lbl()),
            Span::styled("Tag: ", lbl()),
            btn_span(CAA_TAGS[m.tag_idx], m.field == AddField::Tag),
            Span::styled(" (\u{2190}/\u{2192})", lbl()),
        ])),
        _ => None,
    };
    if let Some(line) = line {
        f.render_widget(Paragraph::new(line), area);
    }
}

/// Same as `draw_type_specific_add_row` but for the Edit modal's fields.
fn draw_type_specific_edit_row(f: &mut Frame, m: &EditModal, area: Rect) {
    let line = match m.current_type() {
        "MX" => Some(Line::from(vec![Span::styled("Priority: ", lbl()), input_span(&m.priority, m.field == EditField::Priority, false, 8), Span::styled("  (lower number = higher priority)", lbl())])),
        "SRV" => Some(Line::from(vec![
            Span::styled("Priority: ", lbl()),
            input_span(&m.priority, m.field == EditField::Priority, false, 6),
            Span::raw("  "),
            Span::styled("Weight: ", lbl()),
            input_span(&m.weight, m.field == EditField::Weight, false, 6),
            Span::raw("  "),
            Span::styled("Port: ", lbl()),
            input_span(&m.port, m.field == EditField::Port, false, 6),
            Span::styled("  (Name like _service._proto)", lbl()),
        ])),
        "CAA" => Some(Line::from(vec![
            Span::styled("Flags: ", lbl()),
            input_span(&m.flags, m.field == EditField::Flags, false, 4),
            Span::styled("  (0-255)   ", lbl()),
            Span::styled("Tag: ", lbl()),
            btn_span(CAA_TAGS[m.tag_idx], m.field == EditField::Tag),
            Span::styled(" (\u{2190}/\u{2192})", lbl()),
        ])),
        _ => None,
    };
    if let Some(line) = line {
        f.render_widget(Paragraph::new(line), area);
    }
}

/// Shared dropdown-list overlay renderer used by the Account and Zone
/// autocomplete popups (both in the background Records screen and inside
/// the Add modal). `anchor` is the row the field lives on; the list opens
/// just below it, offset `x_off` columns in.
fn render_dropdown(f: &mut Frame, items: &[String], selected_idx: usize, anchor: Rect, x_off: u16, bounds: Rect) {
    if items.is_empty() {
        return;
    }
    let list_items: Vec<ListItem> = items.iter().map(|s| ListItem::new(s.clone())).collect();
    let list = List::new(list_items)
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(BORDER)))
        .highlight_style(Style::default().fg(BG).bg(FG))
        .style(Style::default().fg(FG).bg(BG2));

    let x = anchor.x + x_off;
    let y = anchor.y + 1;
    let height = (items.len() as u16 + 2).min(10);
    let width = 34u16.min(bounds.width.saturating_sub(x));
    let dd_area = Rect::new(x, y, width, height);

    let mut state = ListState::default();
    state.select(Some(selected_idx));
    f.render_widget(Clear, dd_area);
    f.render_stateful_widget(list, dd_area, &mut state);
}

fn filtered_accounts(cfg: &config::Config, query: &str) -> Vec<String> {
    let q = query.to_lowercase();
    cfg.accounts.iter().map(|a| a.label.clone()).filter(|name| q.is_empty() || name.to_lowercase().contains(&q)).collect()
}

fn filtered_zones(known: &[Zone], query: &str) -> Vec<String> {
    let q = query.to_lowercase();
    known.iter().map(|z| z.name.clone()).filter(|name| q.is_empty() || name.to_lowercase().contains(&q)).collect()
}
