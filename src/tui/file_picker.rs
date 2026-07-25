//! A small interactive file browser modal, used wherever a screen needs the
//! user to pick a path (SSH private key files, mainly) instead of typing
//! one blind.

use std::{fs, path::PathBuf};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};

use super::widgets::*;

pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

pub struct FilePicker {
    pub cwd: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub error: Option<String>,
}

impl FilePicker {
    /// Opens centered on `start_path` — if it's a file, starts in its
    /// parent directory with that file pre-selected; if it's a directory,
    /// starts there; otherwise falls back to `~/.ssh` (or home, or `/`).
    pub fn new(start_path: &str) -> Self {
        let expanded = crate::config::expand_path(start_path);
        let p = PathBuf::from(&expanded);

        let (cwd, preselect) = if p.is_dir() {
            (p, None)
        } else if let Some(parent) = p.parent().filter(|par| par.is_dir()) {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
            (parent.to_path_buf(), name)
        } else {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
            let ssh_dir = home.join(".ssh");
            (if ssh_dir.is_dir() { ssh_dir } else { home }, None)
        };

        let mut picker = Self { cwd, entries: Vec::new(), selected: 0, error: None };
        picker.reload();
        if let Some(name) = preselect {
            if let Some(idx) = picker.entries.iter().position(|e| e.name == name) {
                picker.selected = idx;
            }
        }
        picker
    }

    pub fn reload(&mut self) {
        let mut dirs_list = Vec::new();
        let mut files_list = Vec::new();
        self.error = None;

        match fs::read_dir(&self.cwd) {
            Ok(read) => {
                for entry in read.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    if is_dir {
                        dirs_list.push(Entry { name, is_dir: true });
                    } else {
                        files_list.push(Entry { name, is_dir: false });
                    }
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        dirs_list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        files_list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

        let mut entries = Vec::new();
        if self.cwd.parent().is_some() {
            entries.push(Entry { name: "..".to_string(), is_dir: true });
        }
        entries.extend(dirs_list);
        entries.extend(files_list);
        self.entries = entries;
        self.selected = 0;
    }

    pub fn up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn down(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    /// Enter on the current selection: descends into a directory (and
    /// returns `None`), or returns `Some(path)` if a file was chosen.
    pub fn activate(&mut self) -> Option<PathBuf> {
        let entry = self.entries.get(self.selected)?;
        if entry.name == ".." {
            if let Some(parent) = self.cwd.parent() {
                self.cwd = parent.to_path_buf();
                self.reload();
            }
            None
        } else if entry.is_dir {
            self.cwd = self.cwd.join(&entry.name);
            self.reload();
            None
        } else {
            Some(self.cwd.join(&entry.name))
        }
    }

    /// Which entry row `(x, y)` falls on, if any — mirrors
    /// `HostPicker::row_at`; `area` must be the same one passed to `draw`.
    pub fn row_at(&self, area: Rect, x: u16, y: u16) -> Option<usize> {
        let width = 84u16.min(area.width.saturating_sub(4));
        let height = 24u16.min(area.height.saturating_sub(2));
        let modal_area = centered_rect(width, height, area);
        let inner = Rect {
            x: modal_area.x + 1,
            y: modal_area.y + 1,
            width: modal_area.width.saturating_sub(2),
            height: modal_area.height.saturating_sub(2),
        };
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

pub fn draw(f: &mut Frame, picker: &FilePicker, area: Rect) {
    let width = 84u16.min(area.width.saturating_sub(4));
    let height = 24u16.min(area.height.saturating_sub(2));
    let modal_area = centered_rect(width, height, area);

    f.render_widget(Clear, modal_area);
    let block = Block::default()
        .title(Span::styled(format!(" Select File — {} ", picker.cwd.display()), Style::default().fg(TITLE)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(BG2));
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(inner);

    if let Some(err) = &picker.error {
        f.render_widget(Paragraph::new(Line::from(Span::styled(format!("can't read this directory: {err}"), Style::default().fg(RED)))), rows[0]);
    } else {
        let items: Vec<ListItem> = picker
            .entries
            .iter()
            .map(|e| {
                let label = if e.is_dir { format!("{}/", e.name) } else { e.name.clone() };
                let style = if e.is_dir { Style::default().fg(ACCENT).bold() } else { Style::default().fg(FG) };
                ListItem::new(Span::styled(label, style))
            })
            .collect();
        let list = List::new(items).highlight_style(focused()).style(Style::default().fg(FG).bg(BG2));
        let mut state = ListState::default();
        if !picker.entries.is_empty() {
            state.select(Some(picker.selected.min(picker.entries.len() - 1)));
        }
        f.render_stateful_widget(list, rows[0], &mut state);
    }

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "\u{2191}\u{2193} navigate  Enter open dir / pick file  Esc cancel",
            lbl(),
        ))),
        rows[1],
    );
}
