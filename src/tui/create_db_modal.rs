//! "Create Database" modal shared by the MySQL / PostgreSQL / ClickHouse
//! screens: a required name plus two engine-specific optional fields
//! (charset/collation, owner/encoding, engine/cluster). The modal only
//! collects input — each screen runs the actual `CREATE DATABASE` on its
//! own backend when `handle_key` returns `CreateDbAction::Submit`.

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::mouse;
use super::widgets::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CreateDbField {
    Name,
    Opt1,
    Opt2,
    BtnCreate,
    BtnCancel,
}

pub enum CreateDbAction {
    None,
    Close,
    Submit,
}

/// Label + hint + default value of one optional field.
pub struct OptSpec {
    pub label: &'static str,
    pub hint: &'static str,
    pub default: &'static str,
}

pub struct CreateDbModal {
    title: &'static str,
    specs: [OptSpec; 2],
    pub name: Input,
    pub opt1: Input,
    pub opt2: Input,
    pub field: CreateDbField,
}

impl CreateDbModal {
    pub fn new(title: &'static str, specs: [OptSpec; 2]) -> Self {
        let opt1 = Input::new(specs[0].default);
        let opt2 = Input::new(specs[1].default);
        Self { title, specs, name: Input::default(), opt1, opt2, field: CreateDbField::Name }
    }

    fn next_field(&mut self) {
        self.field = match self.field {
            CreateDbField::Name => CreateDbField::Opt1,
            CreateDbField::Opt1 => CreateDbField::Opt2,
            CreateDbField::Opt2 => CreateDbField::BtnCreate,
            CreateDbField::BtnCreate => CreateDbField::BtnCancel,
            CreateDbField::BtnCancel => CreateDbField::Name,
        };
    }

    fn prev_field(&mut self) {
        self.field = match self.field {
            CreateDbField::Name => CreateDbField::BtnCancel,
            CreateDbField::Opt1 => CreateDbField::Name,
            CreateDbField::Opt2 => CreateDbField::Opt1,
            CreateDbField::BtnCreate => CreateDbField::Opt2,
            CreateDbField::BtnCancel => CreateDbField::BtnCreate,
        };
    }

    fn input_mut(&mut self) -> Option<&mut Input> {
        match self.field {
            CreateDbField::Name => Some(&mut self.name),
            CreateDbField::Opt1 => Some(&mut self.opt1),
            CreateDbField::Opt2 => Some(&mut self.opt2),
            _ => None,
        }
    }

    /// Trimmed values: `(name, opt1, opt2)`.
    pub fn values(&self) -> (String, String, String) {
        (
            self.name.value().trim().to_string(),
            self.opt1.value().trim().to_string(),
            self.opt2.value().trim().to_string(),
        )
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> CreateDbAction {
        match key.code {
            KeyCode::Esc => return CreateDbAction::Close,
            KeyCode::Tab | KeyCode::Down => self.next_field(),
            KeyCode::BackTab | KeyCode::Up => self.prev_field(),
            KeyCode::Enter => match self.field {
                CreateDbField::BtnCreate => return CreateDbAction::Submit,
                CreateDbField::BtnCancel => return CreateDbAction::Close,
                _ => self.next_field(),
            },
            code => {
                if let Some(input) = self.input_mut() {
                    match code {
                        KeyCode::Char(c) => input.insert(c),
                        KeyCode::Backspace => input.backspace(),
                        KeyCode::Delete => input.delete(),
                        KeyCode::Left => input.left(),
                        KeyCode::Right => input.right(),
                        KeyCode::Home => input.home(),
                        KeyCode::End => input.end_of_line(),
                        _ => {}
                    }
                }
            }
        }
        CreateDbAction::None
    }

    /// Clicks focus a field or press Create/Cancel. `area` must be the same
    /// full-screen area passed to `draw`.
    pub fn handle_mouse(&mut self, me: &MouseEvent, area: Rect) -> CreateDbAction {
        let Some((x, y)) = mouse::left_click(me) else { return CreateDbAction::None };
        let rows = layout(area);
        for (i, field) in [(0, CreateDbField::Name), (2, CreateDbField::Opt1), (5, CreateDbField::Opt2)] {
            if mouse::in_rect(rows[i], x, y) {
                self.field = field;
                return CreateDbAction::None;
            }
        }
        match mouse::button_row_hit(x, y, rows[9], &["Create", "Cancel"]) {
            Some(0) => CreateDbAction::Submit,
            Some(_) => CreateDbAction::Close,
            None => CreateDbAction::None,
        }
    }
}

const LABEL_W: usize = 12;

fn modal_rect(area: Rect) -> Rect {
    let width = 70u16.min(area.width.saturating_sub(4));
    // 12 content rows + border(2) + margin(2).
    let height = 16u16.min(area.height.saturating_sub(2));
    centered_rect(width, height, area)
}

fn layout(area: Rect) -> Vec<Rect> {
    let inner = Block::default().borders(Borders::ALL).inner(modal_rect(area));
    Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // 0 Name
            Constraint::Length(1), // 1 spacer
            Constraint::Length(1), // 2 Opt1
            Constraint::Length(1), // 3 hint
            Constraint::Length(1), // 4 spacer
            Constraint::Length(1), // 5 Opt2
            Constraint::Length(1), // 6 hint
            Constraint::Length(1), // 7 spacer
            Constraint::Length(1), // 8 spacer
            Constraint::Length(1), // 9 buttons
            Constraint::Length(1), // 10 spacer
            Constraint::Length(1), // 11 nav hint
            Constraint::Min(0),
        ])
        .split(inner)
        .to_vec()
}

pub fn draw(f: &mut Frame, m: &CreateDbModal, area: Rect) {
    let modal_area = modal_rect(area);
    f.render_widget(Clear, modal_area);
    let block = Block::default()
        .title(Span::styled(format!(" {} ", m.title), Style::default().fg(title_color())))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent()))
        .style(Style::default().bg(bg2()));
    f.render_widget(block, modal_area);

    let rows = layout(area);
    let fw = rows[0].width.saturating_sub(LABEL_W as u16) as usize;
    let field = |label: &str, input: &Input, focused: bool| {
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{label:<width$}", width = LABEL_W), lbl()),
            input_span(input, focused, false, fw),
        ]))
    };

    f.render_widget(field("Name:", &m.name, m.field == CreateDbField::Name), rows[0]);
    f.render_widget(field(m.specs[0].label, &m.opt1, m.field == CreateDbField::Opt1), rows[2]);
    f.render_widget(Paragraph::new(Line::from(Span::styled(m.specs[0].hint, lbl()))), rows[3]);
    f.render_widget(field(m.specs[1].label, &m.opt2, m.field == CreateDbField::Opt2), rows[5]);
    f.render_widget(Paragraph::new(Line::from(Span::styled(m.specs[1].hint, lbl()))), rows[6]);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            btn_span("Create", m.field == CreateDbField::BtnCreate),
            Span::raw("  "),
            btn_span("Cancel", m.field == CreateDbField::BtnCancel),
        ])),
        rows[9],
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled("Tab navigate  \u{2022}  Enter activate  \u{2022}  Esc cancel", lbl()))),
        rows[11],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

    fn modal() -> CreateDbModal {
        CreateDbModal::new(
            "Test",
            [
                OptSpec { label: "A:", hint: "", default: "dflt" },
                OptSpec { label: "B:", hint: "", default: "" },
            ],
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(m: &mut CreateDbModal, s: &str) {
        for c in s.chars() {
            assert!(matches!(m.handle_key(key(KeyCode::Char(c))), CreateDbAction::None));
        }
    }

    #[test]
    fn prefills_defaults_and_trims_values() {
        let mut m = modal();
        type_str(&mut m, "  shop ");
        m.handle_key(key(KeyCode::Tab));
        m.handle_key(key(KeyCode::Tab));
        type_str(&mut m, "x");
        assert_eq!(m.values(), ("shop".to_string(), "dflt".to_string(), "x".to_string()));
    }

    #[test]
    fn enter_on_buttons_submits_or_closes() {
        let mut m = modal();
        // Enter on an input just advances focus.
        assert!(matches!(m.handle_key(key(KeyCode::Enter)), CreateDbAction::None));
        assert!(m.field == CreateDbField::Opt1);
        m.handle_key(key(KeyCode::BackTab));
        m.handle_key(key(KeyCode::BackTab));
        assert!(m.field == CreateDbField::BtnCancel);
        assert!(matches!(m.handle_key(key(KeyCode::Enter)), CreateDbAction::Close));
        m.handle_key(key(KeyCode::Up));
        assert!(m.field == CreateDbField::BtnCreate);
        assert!(matches!(m.handle_key(key(KeyCode::Enter)), CreateDbAction::Submit));
        assert!(matches!(m.handle_key(key(KeyCode::Esc)), CreateDbAction::Close));
    }

    #[test]
    fn mouse_hits_fields_and_buttons() {
        let area = Rect::new(0, 0, 100, 40);
        let rows = layout(area);
        let click = |x: u16, y: u16| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        let mut m = modal();
        assert!(matches!(m.handle_mouse(&click(rows[5].x + 1, rows[5].y), area), CreateDbAction::None));
        assert!(m.field == CreateDbField::Opt2);
        // "[ Create ]" starts at the row's left edge; "[ Cancel ]" follows a 2-col gap.
        assert!(matches!(m.handle_mouse(&click(rows[9].x + 1, rows[9].y), area), CreateDbAction::Submit));
        assert!(matches!(m.handle_mouse(&click(rows[9].x + 13, rows[9].y), area), CreateDbAction::Close));
    }
}
