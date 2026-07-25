//! A multi-select "pill" picker for SQL privileges (`SELECT`, `INSERT`,
//! `UPDATE`, ... or `ALL`) — used by the MySQL/PostgreSQL "Add User"
//! modals so granting a user access means picking exactly what they can
//! do, not just an all-or-nothing `ALL PRIVILEGES`.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::widgets::*;

pub struct PrivPicker {
    /// `items[0]` is always the "ALL" sentinel — selecting it clears every
    /// other flag and vice versa, so the two modes can't fight each other.
    pub items: Vec<&'static str>,
    pub selected: Vec<bool>,
    pub cursor: usize,
}

impl PrivPicker {
    pub fn new(items: &[&'static str]) -> Self {
        let mut selected = vec![false; items.len()];
        if !selected.is_empty() {
            selected[0] = true;
        }
        Self { items: items.to_vec(), selected, cursor: 0 }
    }

    pub fn left(&mut self) {
        if self.cursor == 0 {
            self.cursor = self.items.len().saturating_sub(1);
        } else {
            self.cursor -= 1;
        }
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1) % self.items.len().max(1);
    }

    pub fn toggle(&mut self) {
        if self.items.is_empty() {
            return;
        }
        if self.cursor == 0 {
            let now_on = !self.selected[0];
            self.selected.iter_mut().for_each(|s| *s = false);
            self.selected[0] = now_on;
        } else {
            self.selected[self.cursor] = !self.selected[self.cursor];
            if self.selected[self.cursor] {
                self.selected[0] = false;
            }
        }
        // Nothing picked at all is meaningless for a GRANT — fall back to ALL.
        if self.selected.iter().all(|s| !*s) {
            self.selected[0] = true;
        }
    }

    /// The chosen privilege keywords, ready to join with `", "` into a
    /// `GRANT <...> ON ...` statement. When `ALL` is selected this is just
    /// `[items[0]]`, which already reads as `GRANT ALL ...` / `GRANT ALL
    /// PRIVILEGES ...` — no special-casing needed at the call site.
    pub fn selected_items(&self) -> Vec<&'static str> {
        self.items.iter().zip(&self.selected).filter(|(_, s)| **s).map(|(n, _)| *n).collect()
    }
}

/// How many terminal rows `draw` will need to lay out `picker.items` at
/// `width` columns — call this while sizing the container *before*
/// drawing, so the reserved area always matches what gets rendered.
pub fn rows_needed(picker: &PrivPicker, width: u16) -> u16 {
    wrap(picker, width).len().max(1) as u16
}

fn wrap(picker: &PrivPicker, width: u16) -> Vec<Vec<usize>> {
    let mut lines: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut width_used: u16 = 0;
    for (i, name) in picker.items.iter().enumerate() {
        let w = label_width(name);
        if width_used + w > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            width_used = 0;
        }
        current.push(i);
        width_used += w;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn label_width(name: &str) -> u16 {
    // "[x] NAME  " — checkbox + space + name + trailing gap.
    (name.chars().count() + 6) as u16
}

pub fn draw(f: &mut Frame, picker: &PrivPicker, is_focused: bool, area: Rect) {
    let rows = wrap(picker, area.width);
    let lines: Vec<Line> = rows
        .iter()
        .map(|row| {
            let spans: Vec<Span> = row
                .iter()
                .map(|&i| {
                    let name = picker.items[i];
                    let checked = picker.selected[i];
                    let is_cursor = is_focused && i == picker.cursor;
                    let label = format!("[{}] {}  ", if checked { "x" } else { " " }, name);
                    let style = if is_cursor {
                        focused()
                    } else if checked {
                        Style::default().fg(green()).bg(bg2())
                    } else {
                        Style::default().fg(fg2()).bg(bg2())
                    };
                    Span::styled(label, style)
                })
                .collect();
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}
