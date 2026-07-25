//! A shared "pick a known host" modal. Every other SSH-backed tool (SSH
//! User Manager, the DB managers' SSH/tunnel side, Logs & Journals) can
//! open this instead of making the user retype a host's connection info
//! from scratch — it lists whatever's already saved in `~/.ssh/config`
//! (the SSH Server Manager's own source of truth) and hands back the
//! picked `Server` so the caller can pull whichever fields it needs.
//!
//! This deliberately does *not* try to unify every tool's connection
//! storage into one schema — a MySQL profile has DB user/password/tunnel
//! fields that have no equivalent in `~/.ssh/config`, and forcing them
//! into one shape would make every tool worse. Instead, only the SSH side
//! (host/port/user/identity file) is ever reused; service-specific fields
//! stay exactly where they are, filled in separately by the user.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::easyssh_mgr::config::{self as easyssh_config, Server};

use super::widgets::*;

pub struct HostPicker {
    pub query: Input,
    all: Vec<Server>,
    pub selected: usize,
    pub error: Option<String>,
}

impl HostPicker {
    pub fn new() -> Self {
        let (all, error) = match easyssh_config::list_servers("") {
            Ok(servers) => (servers, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        Self { query: Input::default(), all, selected: 0, error }
    }

    fn filtered(&self) -> Vec<&Server> {
        let q = self.query.value().trim().to_lowercase();
        self.all
            .iter()
            .filter(|s| {
                q.is_empty()
                    || s.alias.to_lowercase().contains(&q)
                    || s.effective_host().to_lowercase().contains(&q)
                    || s.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .collect()
    }

    pub fn up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn down(&mut self) {
        if self.selected + 1 < self.filtered().len() {
            self.selected += 1;
        }
    }

    pub fn insert(&mut self, c: char) {
        self.query.insert(c);
        self.selected = 0;
    }

    pub fn backspace(&mut self) {
        self.query.backspace();
        self.selected = 0;
    }

    /// The currently-selected server, cloned out so the caller is free to
    /// drop this picker (and its borrow of `self.all`) right after.
    pub fn activate(&self) -> Option<Server> {
        self.filtered().get(self.selected).map(|s| (*s).clone())
    }

    /// Which list row `(x, y)` falls on, if any — callers use this to turn
    /// a click straight into a pick (select + activate in one motion),
    /// mirroring what `Up`/`Up`.../`Enter` would do. `area` must be the
    /// same one passed to `draw`.
    pub fn row_at(&self, area: Rect, x: u16, y: u16) -> Option<usize> {
        let list_area = self.list_rect(area);
        if x <= list_area.x || x + 1 >= list_area.x + list_area.width {
            return None;
        }
        if y < list_area.y || y >= list_area.y + list_area.height {
            return None;
        }
        let idx = (y - list_area.y) as usize;
        if idx < self.filtered().len() {
            Some(idx)
        } else {
            None
        }
    }

    fn list_rect(&self, area: Rect) -> Rect {
        let width = 84u16.min(area.width.saturating_sub(4));
        let height = 22u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);
        let inner = Rect {
            x: modal_area.x + 1,
            y: modal_area.y + 1,
            width: modal_area.width.saturating_sub(2),
            height: modal_area.height.saturating_sub(2),
        };
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)])
            .split(inner);
        rows[2]
    }
}

impl Default for HostPicker {
    fn default() -> Self {
        Self::new()
    }
}

pub fn draw(f: &mut Frame, picker: &HostPicker, area: Rect) {
    let width = 84u16.min(area.width.saturating_sub(4));
    let height = 22u16.min(area.height.saturating_sub(2));
    let modal_area = centered_rect(width, height, area);
    f.render_widget(Clear, modal_area);
    let block = Block::default()
        .title(Span::styled(" Pick a Known Host (from SSH Server Manager) ", Style::default().fg(title_color())))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent()))
        .style(Style::default().bg(bg2()));
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)])
        .split(inner);
    let fw = rows[0].width.saturating_sub(10) as usize;
    f.render_widget(Paragraph::new(Line::from(vec![Span::styled("Search: ", lbl()), input_span(&picker.query, true, false, fw)])), rows[0]);

    if let Some(err) = &picker.error {
        f.render_widget(Paragraph::new(Line::from(Span::styled(format!("couldn't read ~/.ssh/config: {err}"), Style::default().fg(red())))), rows[2]);
    } else {
        let filtered = picker.filtered();
        if filtered.is_empty() {
            let msg = if picker.all.is_empty() {
                "no hosts saved yet — add one in the SSH Server Manager first"
            } else {
                "no saved hosts match"
            };
            f.render_widget(Paragraph::new(Line::from(Span::styled(msg, lbl()))), rows[2]);
        } else {
            let items: Vec<ListItem> = filtered
                .iter()
                .map(|s| {
                    let user_part = if s.user.is_empty() { String::new() } else { format!("{}@", s.user) };
                    let tags_part = if s.tags.is_empty() { String::new() } else { format!("  [{}]", s.tags.join(", ")) };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{:<22}", s.alias), Style::default().fg(fg()).add_modifier(Modifier::BOLD)),
                        Span::styled(format!(" {user_part}{}", s.effective_host()), Style::default().fg(fg2())),
                        Span::styled(tags_part, Style::default().fg(accent())),
                    ]))
                })
                .collect();
            let list = List::new(items).highlight_style(focused()).style(Style::default().fg(fg()).bg(bg2()));
            let mut state = ListState::default();
            state.select(Some(picker.selected.min(filtered.len() - 1)));
            f.render_stateful_widget(list, rows[2], &mut state);
        }
    }

    f.render_widget(
        Paragraph::new(Line::from(Span::styled("type to filter  \u{2191}\u{2193} navigate  Enter pick  Esc cancel", lbl()))),
        rows[3],
    );
}
