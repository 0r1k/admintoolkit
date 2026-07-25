//! Shared TUI building blocks (palette, text input, buttons, modal) reused
//! by every module screen so the whole app feels like one tool.

use arboard::Clipboard;
use rand::Rng;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

// ── Palette ──────────────────────────────────────────────────────────────
// Each of these reads the current theme (see `theme.rs`) fresh every call,
// rather than being fixed constants — that's what makes `F9`-cycling the
// theme repaint every screen immediately. The default theme, `Classic`, is
// atk's original hand-picked palette verbatim, so a fresh install (or a
// missing/corrupt theme.json) looks exactly like it always did.
use super::theme;

pub fn bg() -> Color {
    theme::palette().bg
}
pub fn bg2() -> Color {
    theme::palette().bg2
}
pub fn bg3() -> Color {
    theme::palette().bg3
}
pub fn border() -> Color {
    theme::palette().border
}
pub fn title_color() -> Color {
    theme::palette().title
}
pub fn fg() -> Color {
    theme::palette().fg
}
pub fn fg2() -> Color {
    theme::palette().fg2
}
pub fn accent() -> Color {
    theme::palette().accent
}
pub fn green() -> Color {
    theme::palette().green
}
pub fn red() -> Color {
    theme::palette().red
}
pub fn yellow() -> Color {
    theme::palette().yellow
}

// ── Input ────────────────────────────────────────────────────────────────
#[derive(Default, Clone)]
pub struct Input {
    pub text: String,
    pub cursor: usize, // char index
}

impl Input {
    pub fn new(s: &str) -> Self {
        Self {
            text: s.to_string(),
            cursor: s.chars().count(),
        }
    }

    pub fn value(&self) -> &str {
        &self.text
    }

    pub fn insert(&mut self, c: char) {
        let byte = self.char_to_byte(self.cursor);
        self.text.insert(byte, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let byte = self.char_to_byte(self.cursor);
            let end = self.text[byte..]
                .chars()
                .next()
                .map(|c| byte + c.len_utf8())
                .unwrap_or(byte);
            self.text.drain(byte..end);
        }
    }

    pub fn delete(&mut self) {
        let byte = self.char_to_byte(self.cursor);
        if byte < self.text.len() {
            let end = self.text[byte..]
                .chars()
                .next()
                .map(|c| byte + c.len_utf8())
                .unwrap_or(byte);
            self.text.drain(byte..end);
        }
    }

    pub fn left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn right(&mut self) {
        if self.cursor < self.text.chars().count() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end_of_line(&mut self) {
        self.cursor = self.text.chars().count();
    }

    fn char_to_byte(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }
}

// ── Widget helpers ──────────────────────────────────────────────────────
pub fn theme_block(title: &str) -> Block<'_> {
    Block::default()
        .title(Span::styled(title, Style::default().fg(title_color())))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border()))
        .style(Style::default().bg(bg()))
}

/// Label text style. Deliberately has no background of its own: labels sit
/// directly on whatever container is behind them (bg() on a full-screen tab,
/// bg2() inside a modal) and must blend into it, not stand out as a box —
/// only input fields (`normal`/`focused`) should look boxed.
pub fn lbl() -> Style {
    Style::default().fg(fg2())
}

/// Focused input field: a solid highlight block, unmistakable regardless of
/// the surrounding container's background — matches the focused-button look
/// so "focused control" reads the same everywhere in the app.
pub fn focused() -> Style {
    Style::default().fg(bg()).bg(accent())
}

/// Unfocused input field. Uses bg3() rather than bg2() so the field's box stays
/// visible against *any* container background, including modals (which are
/// themselves bg2()) — otherwise an empty unfocused field is invisible.
pub fn normal() -> Style {
    Style::default().fg(fg()).bg(bg3())
}

/// Render a fixed-width input field that scrolls to keep the cursor visible.
/// `field_w` is the exact number of columns the span must occupy.
pub fn input_span(input: &Input, is_focused: bool, password: bool, field_w: usize) -> Span<'static> {
    let field_w = field_w.max(2);
    let source: Vec<char> = if password {
        vec!['*'; input.text.chars().count()]
    } else {
        input.text.chars().collect()
    };
    let total = source.len();
    let cursor = input.cursor.min(total);

    let text_w = if is_focused { field_w - 1 } else { field_w };
    let scroll = if cursor >= text_w { cursor - text_w + 1 } else { 0 };
    let vis_end = (scroll + text_w).min(total);
    let visible: &[char] = if scroll <= vis_end { &source[scroll..vis_end] } else { &[] };

    let mut buf = String::with_capacity(field_w + 1);
    if is_focused {
        let cur_in_view = cursor - scroll;
        for (i, &c) in visible.iter().enumerate() {
            if i == cur_in_view {
                buf.push('\u{2502}');
            }
            buf.push(c);
        }
        if cur_in_view >= visible.len() {
            buf.push('\u{2502}');
        }
    } else {
        buf.extend(visible);
    }
    let len = buf.chars().count();
    if len < field_w {
        buf.extend(std::iter::repeat(' ').take(field_w - len));
    }

    Span::styled(buf, if is_focused { focused() } else { normal() })
}

pub fn btn_span(label: &str, focused: bool) -> Span<'static> {
    let text = format!("[ {label} ]");
    if focused {
        Span::styled(
            text,
            Style::default()
                .fg(bg())
                .bg(accent())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(text, Style::default().fg(fg2()).bg(bg3()))
    }
}

pub fn tab_span(label: &str, active: bool) -> Span<'static> {
    if active {
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(accent())
                .bg(bg())
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
    } else {
        Span::styled(label.to_string(), Style::default().fg(fg2()).bg(bg()))
    }
}

/// A `width` x `height` rect centered within `area`, clamped so it never
/// overflows. Used to place custom (non-text) modal dialogs.
pub fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

pub fn draw_modal(f: &mut Frame, title: &str, msg: &str, area: Rect) {
    let width = 60u16.min(area.width.saturating_sub(4));
    let height = (msg.lines().count() as u16 + 5).min(area.height.saturating_sub(2)).max(6);
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    let modal_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, modal_area);
    f.render_widget(
        Paragraph::new(format!("{msg}\n\nEnter / Esc to close"))
            .block(
                Block::default()
                    .title(Span::styled(format!(" {title} "), Style::default().fg(title_color())))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(accent())),
            )
            .style(Style::default().fg(fg()).bg(bg2()))
            .wrap(Wrap { trim: true }),
        modal_area,
    );
}

/// The "History" log panel every screen has at least one of. Pinned to
/// the newest line by default (`scroll_up_offset == 0`); scrolling up
/// increases the offset to look back at older entries without losing
/// track of new ones arriving below — the offset is *from the bottom*,
/// so it stays meaningful as more lines get appended while scrolled up.
pub fn draw_history(f: &mut Frame, history: &[(bool, String)], area: Rect, scroll_up_offset: u16) {
    let block = theme_block(" History (Ctrl+\u{2191}/\u{2193} scroll, Ctrl+Y copy) ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines: Vec<Line> = history.iter().map(|(ok, line)| Line::from(Span::styled(line.as_str(), Style::default().fg(if *ok { green() } else { red() })))).collect();
    let total = lines.len() as u16;
    let visible = inner.height;
    let max_offset = total.saturating_sub(visible);
    let offset = scroll_up_offset.min(max_offset);
    let scroll = max_offset - offset;
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((scroll, 0)), inner);
}

/// Copies the whole History panel (not just what's currently scrolled into
/// view) to the system clipboard as newline-joined plain text, since mouse
/// capture (needed for click/scroll support) blocks the terminal's own
/// native text selection in most terminals.
pub fn copy_history_to_clipboard(history: &[(bool, String)]) -> bool {
    if history.is_empty() {
        return false;
    }
    let text = history.iter().map(|(_, line)| line.as_str()).collect::<Vec<_>>().join("\n");
    Clipboard::new().and_then(|mut c| c.set_text(text)).is_ok()
}

/// A random 20-char alphanumeric password, used by the "Generate" button in
/// Add User modals.
pub fn generate_password() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..20).map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char).collect()
}

pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
