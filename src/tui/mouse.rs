//! Small shared helpers for mouse support. Each screen does its own hit
//! testing (mostly by recomputing the same fixed-size `Layout::split`
//! rows its `draw()` uses) rather than going through a generic widget
//! tree, since every screen here is a handful of predictable vertical
//! stacks, not an arbitrary UI.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

/// `(column, row)` of a left-button press, if that's what this event is.
pub fn left_click(me: &MouseEvent) -> Option<(u16, u16)> {
    match me.kind {
        MouseEventKind::Down(MouseButton::Left) => Some((me.column, me.row)),
        _ => None,
    }
}

/// -1 for scroll-up, +1 for scroll-down, if this event is a wheel scroll.
pub fn scroll_delta(me: &MouseEvent) -> Option<i32> {
    match me.kind {
        MouseEventKind::ScrollUp => Some(-1),
        MouseEventKind::ScrollDown => Some(1),
        _ => None,
    }
}

/// The content area inside a `Block::bordered()` (every themed block in
/// this codebase uses `Borders::ALL`), without needing to construct the
/// actual `Block` just to call `.inner()`.
pub fn block_inner(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

pub fn in_rect(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.x.saturating_add(rect.width) && y >= rect.y && y < rect.y.saturating_add(rect.height)
}

/// Which of a row of labels (tab bars via `tab_span`, button rows via
/// `btn_span` — both get joined with a two-space gap throughout this
/// codebase) a click landed on, if any. `row` is the `Rect` the labels
/// were drawn into (its `.x`/`.y` anchor the first label); `labels` must
/// be in the same left-to-right order they were drawn in.
pub fn label_row_hit(x: u16, y: u16, row: Rect, labels: &[&str]) -> Option<usize> {
    if y != row.y || x < row.x {
        return None;
    }
    let mut col = row.x;
    for (i, label) in labels.iter().enumerate() {
        let w = label.chars().count() as u16;
        if x >= col && x < col + w {
            return Some(i);
        }
        col += w + 2; // the "  " gap every label row in this codebase uses
    }
    None
}

/// Like [`label_row_hit`], but for a row of `btn_span` buttons — each
/// gets wrapped in `"[ … ]"` before measuring, matching what `btn_span`
/// actually renders (`tab_span`, used for tab bars, renders bare text —
/// that's what `label_row_hit` is for).
pub fn button_row_hit(x: u16, y: u16, row: Rect, labels: &[&str]) -> Option<usize> {
    let bracketed: Vec<String> = labels.iter().map(|l| format!("[ {l} ]")).collect();
    let refs: Vec<&str> = bracketed.iter().map(String::as_str).collect();
    label_row_hit(x, y, row, &refs)
}

/// Like [`table_row_hit`], for a plain multi-line `Paragraph` with no
/// border/header of its own (e.g. a hand-rendered "list" of `Line`s) —
/// row 0 starts right at `area`'s top edge instead of one cell in.
pub fn plain_row_hit(x: u16, y: u16, area: Rect, row_count: usize) -> Option<usize> {
    if row_count == 0 || x < area.x || x >= area.x + area.width || y < area.y || y >= area.y + area.height {
        return None;
    }
    let idx = (y - area.y) as usize;
    if idx < row_count {
        Some(idx)
    } else {
        None
    }
}

/// Row index within a bordered `Table`/`List` block (1 border + optional
/// header rows of content above the data), if `(x, y)` lands inside its
/// data area. `header_rows` is 1 for a `Table` with `.header(..)`, 0 for a
/// plain `List`.
///
/// `selected` must be the same index passed to `TableState::select`/
/// `ListState::select` for this render. Every screen builds a fresh
/// `TableState`/`ListState` each frame (offset always starts at 0), so
/// ratatui's auto-scroll-to-keep-selection-visible lands on a scroll
/// offset that's a pure function of `(selected, visible_rows, row_count)`
/// — `max(0, selected - visible_rows + 1)`, since every row in this
/// codebase is a single line tall. Without replicating that here, a click
/// on any row past the first screenful landed on `y - top` (i.e. as if
/// the table were unscrolled), selecting the wrong item entirely once a
/// selection scrolled the view down — this was the "click one row, get a
/// different one" bug.
pub fn table_row_hit(x: u16, y: u16, table_area: Rect, header_rows: u16, row_count: usize, selected: usize) -> Option<usize> {
    if row_count == 0 {
        return None;
    }
    if x <= table_area.x || x + 1 >= table_area.x + table_area.width {
        return None;
    }
    let top = table_area.y + 1 + header_rows;
    let bottom = table_area.y + table_area.height.saturating_sub(1);
    if y < top || y >= bottom {
        return None;
    }
    let visible = (bottom - top) as usize;
    let selected = selected.min(row_count - 1);
    let offset = selected.saturating_sub(visible.saturating_sub(1));
    let idx = offset + (y - top) as usize;
    if idx < row_count {
        Some(idx)
    } else {
        None
    }
}
