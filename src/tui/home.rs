use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::config::config_file;

use super::widgets::*;
use super::Screen;

struct HomeItem {
    title: &'static str,
    desc: &'static str,
    bin: &'static str,
    screen: Screen,
}

/// Canonical tool list and their default order — what a fresh install (or
/// a corrupted/missing `menu_order.json`) falls back to.
const DEFAULT_ITEMS: &[HomeItem] = &[
    HomeItem {
        title: "SSH Server Manager",
        desc: "Browse/add/edit ~/.ssh/config hosts, tag & group servers, connect, pin, port-forward",
        bin: "easyssh",
        screen: Screen::EasySsh,
    },
    HomeItem {
        title: "SSH User Manager",
        desc: "Provision / remove Linux users + SSH keys on remote hosts",
        bin: "linux-ssh-user-manager",
        screen: Screen::SshUser,
    },
    HomeItem {
        title: "Cloudflare DNS Manager",
        desc: "Manage DNS records across multiple Cloudflare accounts — real per-record IDs, Proxied toggle",
        bin: "cloudflare-dns",
        screen: Screen::Cloudflare,
    },
    HomeItem {
        title: "GoDaddy DNS Manager",
        desc: "Manage DNS records across multiple GoDaddy API accounts",
        bin: "domain-api",
        screen: Screen::GoDaddy,
    },
    HomeItem {
        title: "MySQL User Manager",
        desc: "Create, list, delete MySQL/MariaDB users — direct or via SSH tunnel",
        bin: "mysql-mgr",
        screen: Screen::Mysql,
    },
    HomeItem {
        title: "PostgreSQL User Manager",
        desc: "Create, list, delete PostgreSQL roles — direct or via SSH tunnel",
        bin: "postgresql-mgr",
        screen: Screen::Postgresql,
    },
    HomeItem {
        title: "ClickHouse User Manager",
        desc: "Create, list, delete ClickHouse users & rotate passwords over SSH",
        bin: "chwm",
        screen: Screen::ClickHouse,
    },
    HomeItem {
        title: "Logs & Journals Reader",
        desc: "SSH in and read journalctl / /var/log files — severity filters, search, live-ish auto-refresh",
        bin: "logread",
        screen: Screen::Logs,
    },
    HomeItem {
        title: "Kernel Tuner",
        desc: "Best-practice sysctl/sysfs/ulimit tuning for desktop, DB, traffic, gaming or AI workloads — local or remote, runtime-only unless you opt into persisting",
        bin: "kerneltune",
        screen: Screen::KernelTune,
    },
    HomeItem {
        title: "SSL Certificate Manager",
        desc: "Detects what's on :443 (nginx/apache, version, domains), shows cert expiry, and safely swaps in a new cert + CA/chain file with a config test before reload",
        bin: "sslcert",
        screen: Screen::SslCert,
    },
];

fn screen_key(s: Screen) -> &'static str {
    match s {
        Screen::Home => "home",
        Screen::EasySsh => "easyssh",
        Screen::SshUser => "sshuser",
        Screen::GoDaddy => "godaddy",
        Screen::Cloudflare => "cloudflare",
        Screen::Mysql => "mysql",
        Screen::Postgresql => "postgresql",
        Screen::ClickHouse => "clickhouse",
        Screen::Logs => "logs",
        Screen::KernelTune => "kerneltune",
        Screen::SslCert => "sslcert",
    }
}

fn screen_from_key(key: &str) -> Option<Screen> {
    match key {
        "easyssh" => Some(Screen::EasySsh),
        "sshuser" => Some(Screen::SshUser),
        "godaddy" => Some(Screen::GoDaddy),
        "cloudflare" => Some(Screen::Cloudflare),
        "mysql" => Some(Screen::Mysql),
        "postgresql" => Some(Screen::Postgresql),
        "clickhouse" => Some(Screen::ClickHouse),
        "logs" => Some(Screen::Logs),
        "kerneltune" => Some(Screen::KernelTune),
        "sslcert" => Some(Screen::SslCert),
        _ => None,
    }
}

/// Greedy word-wrap: as many whole words as fit per line at `width`
/// columns. Used instead of a `Paragraph`'s built-in `Wrap` because these
/// descriptions live inside `List` rows (for the highlight-whole-item
/// selection style every other row-based screen uses), and `List` never
/// reflows a `Line`'s text itself — it only ever clips at the container's
/// edge, which is what left descriptions truncated instead of wrapping
/// when the terminal was narrower than the text.
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.chars().count() + 1 + word.chars().count() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

const DESC_INDENT: &str = "     ";

/// Row count (title line + wrapped description lines + one blank spacer)
/// each item in `order` will actually render at, for a list content area
/// `desc_width` columns wide (i.e. already excluding the block's border
/// and `DESC_INDENT`). `draw` and `HomeState::handle_mouse` both call this
/// with the same inputs so a click always lands on the item it looks like
/// it landed on, regardless of how the description happened to wrap.
fn item_heights(order: &[Screen], desc_width: usize) -> Vec<usize> {
    order
        .iter()
        .map(|s| {
            let item = DEFAULT_ITEMS.iter().find(|i| i.screen == *s).expect("every Screen in `order` exists in DEFAULT_ITEMS");
            1 + wrap_text(item.desc, desc_width).len() + 1
        })
        .collect()
}

fn order_path() -> PathBuf {
    config_file("menu_order.json")
}

/// Loads the user's saved tool order, falling back to `DEFAULT_ITEMS`'
/// order when there's no saved file, it's corrupt, or every entry in it
/// failed to resolve. Any tool missing from a saved order (e.g. one added
/// in a later atk version, after the user last reordered) is appended at
/// the end in its default position rather than silently disappearing.
fn load_order() -> Vec<Screen> {
    let defaults: Vec<Screen> = DEFAULT_ITEMS.iter().map(|i| i.screen).collect();
    let mut order: Vec<Screen> = std::fs::read_to_string(order_path())
        .ok()
        .and_then(|data| serde_json::from_str::<Vec<String>>(&data).ok())
        .map(|keys| {
            let mut seen = Vec::new();
            for key in &keys {
                if let Some(screen) = screen_from_key(key) {
                    if !seen.contains(&screen) {
                        seen.push(screen);
                    }
                }
            }
            seen
        })
        .unwrap_or_default();

    for screen in defaults {
        if !order.contains(&screen) {
            order.push(screen);
        }
    }
    order
}

fn save_order(order: &[Screen]) {
    let keys: Vec<&str> = order.iter().map(|s| screen_key(*s)).collect();
    if let Ok(json) = serde_json::to_string_pretty(&keys) {
        let _ = std::fs::write(order_path(), json);
    }
}

pub struct HomeState {
    pub selected: usize,
    order: Vec<Screen>,
}

impl HomeState {
    pub fn new() -> Self {
        Self { selected: 0, order: load_order() }
    }

    fn item(&self, screen: Screen) -> &'static HomeItem {
        DEFAULT_ITEMS.iter().find(|i| i.screen == screen).expect("every Screen in `order` exists in DEFAULT_ITEMS")
    }

    /// Swaps the selected tool with its neighbor `delta` slots away (-1 up,
    /// +1 down) and persists the new order immediately, so a reorder can't
    /// be lost by forgetting to "save" — there's no separate save step.
    fn move_selected(&mut self, delta: isize) {
        let len = self.order.len() as isize;
        let new_idx = self.selected as isize + delta;
        if new_idx < 0 || new_idx >= len {
            return;
        }
        self.order.swap(self.selected, new_idx as usize);
        self.selected = new_idx as usize;
        save_order(&self.order);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Screen> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.order.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Char('K') => self.move_selected(-1),
            KeyCode::Char('J') => self.move_selected(1),
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if let Some(n) = c.to_digit(10) {
                    let idx = n as usize;
                    if idx >= 1 && idx <= self.order.len() {
                        return Some(self.order[idx - 1]);
                    }
                }
            }
            KeyCode::Enter => return Some(self.order[self.selected]),
            _ => {}
        }
        None
    }

    /// A click on a tool row selects and opens it in one motion, the way a
    /// launcher list would — there's no separate "select then confirm"
    /// step to mouse users elsewhere in the app either.
    pub fn handle_mouse(&mut self, me: MouseEvent, area: Rect) -> Option<Screen> {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(0), Constraint::Length(1)])
            .split(area);

        if let Some(delta) = super::mouse::scroll_delta(&me) {
            if super::mouse::in_rect(chunks[1], me.column, me.row) {
                if delta < 0 && self.selected > 0 {
                    self.selected -= 1;
                } else if delta > 0 && self.selected + 1 < self.order.len() {
                    self.selected += 1;
                }
            }
            return None;
        }

        let (x, y) = super::mouse::left_click(&me)?;
        let list_area = chunks[1];
        if x < list_area.x + 1 || x + 1 >= list_area.x + list_area.width {
            return None;
        }
        if y < list_area.y + 1 || y + 1 >= list_area.y + list_area.height {
            return None;
        }
        let desc_width = (list_area.width as usize).saturating_sub(2).saturating_sub(DESC_INDENT.len()).max(10);
        let heights = item_heights(&self.order, desc_width);
        let mut rel_y = (y - (list_area.y + 1)) as usize;
        for (idx, h) in heights.iter().enumerate() {
            if rel_y < *h {
                self.selected = idx;
                return Some(self.order[idx]);
            }
            rel_y -= h;
        }
        None
    }
}

pub fn draw(f: &mut Frame, state: &HomeState, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let banner = vec![
        Line::from(Span::styled(
            "atk — Admin Toolkit",
            Style::default().fg(title_color()).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "A sysadmin's Swiss Army knife: SSH servers, SSH users, DNS, MySQL, PostgreSQL, ClickHouse, logs, kernel tuning — one TUI",
            Style::default().fg(fg2()),
        )),
        Line::from(Span::styled(format!("or1k.net  \u{2022}  theme: {}", super::theme::current().label()), Style::default().fg(border()))),
    ];
    f.render_widget(
        Paragraph::new(banner).alignment(Alignment::Center),
        chunks[0],
    );

    let desc_width = (chunks[1].width as usize).saturating_sub(2).saturating_sub(DESC_INDENT.len()).max(10);
    let items: Vec<ListItem> = state
        .order
        .iter()
        .enumerate()
        .map(|(i, screen)| {
            let item = state.item(*screen);
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("  {}. ", i + 1), Style::default().fg(fg2())),
                Span::styled(item.title, Style::default().fg(fg()).add_modifier(Modifier::BOLD)),
                Span::styled(format!("  ({})", item.bin), Style::default().fg(fg2())),
            ])];
            for w in wrap_text(item.desc, desc_width) {
                lines.push(Line::from(Span::styled(format!("{DESC_INDENT}{w}"), Style::default().fg(fg2()))));
            }
            lines.push(Line::from(""));
            ListItem::new(lines)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .title(Span::styled(" Tools ", Style::default().fg(title_color())))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border())),
        )
        .style(Style::default().bg(bg()))
        .highlight_style(Style::default().bg(bg3()))
        .highlight_symbol(" \u{25B6} ");

    let mut lstate = ListState::default();
    lstate.select(Some(state.selected));
    f.render_stateful_widget(list, chunks[1], &mut lstate);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "\u{2191}\u{2193}/1-9 select  Enter open  Shift+K/J move up/down  F9 theme  F12 mouse  Ctrl+C quit",
            lbl(),
        )))
        .alignment(Alignment::Center),
        chunks[2],
    );
}
