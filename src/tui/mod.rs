pub mod widgets;

mod clickhouse_screen;
mod easyssh_screen;
mod file_picker;
mod godaddy_screen;
mod home;
mod host_picker;
mod kerneltune_screen;
mod logs_screen;
mod mouse;
mod mysql_screen;
mod postgresql_screen;
mod priv_picker;
mod sshuser_screen;

use std::{io, time::{Duration, Instant}};

use crossterm::{
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEvent, KeyModifiers, MouseEvent},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, layout::Rect, style::Style, widgets::Block, Terminal};

use widgets::BG;

/// Enables only what this app actually handles: click press/release
/// (`1000`) and SGR-encoded coordinates for terminals wider than 223
/// columns (`1006`). Deliberately narrower than
/// `crossterm::event::EnableMouseCapture`, which also turns on
/// button-motion (`1002`) and any-motion (`1003`) tracking — those report
/// every pixel of mouse movement as its own event, and under real
/// mouse+keyboard use that floods the input stream badly enough that
/// keyboard presses queue up behind however many Moved events arrived
/// first, reading as "keyboard navigation stopped working". This app
/// never handles `Drag`/`Moved`, only `Down`/`Up`/`Scroll*`, so there's
/// nothing lost by leaving those two modes off.
struct EnableMouseCaptureMinimal;

impl crossterm::Command for EnableMouseCaptureMinimal {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[?1000h\x1b[?1006h")
    }
}

struct DisableMouseCaptureMinimal;

impl crossterm::Command for DisableMouseCaptureMinimal {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[?1006l\x1b[?1000l")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Home,
    EasySsh,
    SshUser,
    GoDaddy,
    Mysql,
    Postgresql,
    ClickHouse,
    Logs,
    KernelTune,
}

/// An action a screen can't perform itself because it needs the terminal
/// taken out of TUI mode first (e.g. handing the real TTY to an interactive
/// `ssh` session).
pub enum PendingAction {
    /// Run this argv (program + args) with inherited stdio while the TUI is
    /// suspended, then resume. `alias` identifies which server this was for
    /// (so last-seen/SSH-count metadata can be recorded once control
    /// returns).
    RunInteractive { program: String, args: Vec<String>, alias: String },
}

struct App {
    screen: Screen,
    home: home::HomeState,
    easyssh: Option<easyssh_screen::EasySshScreen>,
    sshuser: Option<sshuser_screen::SshUserScreen>,
    godaddy: Option<godaddy_screen::GoDaddyScreen>,
    mysql: Option<mysql_screen::MysqlScreen>,
    postgresql: Option<postgresql_screen::PostgresqlScreen>,
    clickhouse: Option<clickhouse_screen::ClickHouseScreen>,
    logs: Option<logs_screen::LogsScreen>,
    kerneltune: Option<kerneltune_screen::KernelTuneScreen>,
    should_quit: bool,
    /// Some real terminals (and some multiplexers sitting between one and
    /// us) report a single physical left-click as more than one `Down`
    /// event — with every screen treating `Down` as "act now", that turns
    /// one click into the same button firing several times in a row.
    /// Earlier this was debounced by requiring a matching `Up` before the
    /// next `Down` counted, but some terminals never send `Up` at all —
    /// that permanently wedged every click after the first. A short time
    /// window at the same position is a more robust signal that two
    /// `Down`s are really one physical click, since it doesn't depend on
    /// any other event ever arriving.
    last_click: Option<(u16, u16, Instant)>,
    /// Whether mouse-reporting escape sequences are currently turned on.
    /// Off by default: the app is keyboard-first, and with capture off the
    /// terminal handles clicks/drags itself, so native text selection and
    /// its usual copy shortcut (Ctrl+Shift+C, Cmd+C, right-click, ...) just
    /// work without the app's involvement. `F12` flips this on for anyone
    /// who wants click-to-select on tabs/buttons/table rows and
    /// wheel-scroll — every one of those is exactly what the equivalent
    /// keypress already does, so turning it on never adds a click-only
    /// capability, only a second way to reach one that already exists.
    /// `run_app` compares this against what the terminal is actually set
    /// to and (de)activates capture on change; it can't be done here since
    /// that needs the terminal handle, which `App` doesn't own.
    mouse_enabled: bool,
}

/// `Down` events at the same cell within this long of each other are
/// treated as one physical click. Long enough to absorb a terminal's
/// repeat reports, short enough that no plausible double-click-on-purpose
/// gets eaten by it.
const CLICK_DEBOUNCE: Duration = Duration::from_millis(150);

impl App {
    fn new() -> Self {
        Self {
            screen: Screen::Home,
            home: home::HomeState::new(),
            easyssh: None,
            sshuser: None,
            godaddy: None,
            mysql: None,
            postgresql: None,
            clickhouse: None,
            logs: None,
            kerneltune: None,
            should_quit: false,
            last_click: None,
            mouse_enabled: false,
        }
    }

    fn enter(&mut self, screen: Screen) {
        match screen {
            Screen::EasySsh => {
                if self.easyssh.is_none() {
                    self.easyssh = Some(easyssh_screen::EasySshScreen::new());
                }
            }
            Screen::SshUser => {
                if self.sshuser.is_none() {
                    self.sshuser = Some(sshuser_screen::SshUserScreen::new());
                }
            }
            Screen::GoDaddy => {
                if self.godaddy.is_none() {
                    self.godaddy = Some(godaddy_screen::GoDaddyScreen::new());
                }
            }
            Screen::Mysql => {
                if self.mysql.is_none() {
                    self.mysql = Some(mysql_screen::MysqlScreen::new());
                }
            }
            Screen::Postgresql => {
                if self.postgresql.is_none() {
                    self.postgresql = Some(postgresql_screen::PostgresqlScreen::new());
                }
            }
            Screen::ClickHouse => {
                if self.clickhouse.is_none() {
                    self.clickhouse = Some(clickhouse_screen::ClickHouseScreen::new());
                }
            }
            Screen::Logs => {
                if self.logs.is_none() {
                    self.logs = Some(logs_screen::LogsScreen::new());
                }
            }
            Screen::KernelTune => {
                if self.kerneltune.is_none() {
                    self.kerneltune = Some(kerneltune_screen::KernelTuneScreen::new());
                }
            }
            Screen::Home => {}
        }
        self.screen = screen;
    }

    fn tick(&mut self) {
        if let Some(s) = &mut self.easyssh {
            s.tick();
        }
        if let Some(s) = &mut self.sshuser {
            s.tick();
        }
        if let Some(s) = &mut self.godaddy {
            s.tick();
        }
        if let Some(s) = &mut self.mysql {
            s.tick();
        }
        if let Some(s) = &mut self.postgresql {
            s.tick();
        }
        if let Some(s) = &mut self.clickhouse {
            s.tick();
        }
        if let Some(s) = &mut self.logs {
            s.tick();
        }
        if let Some(s) = &mut self.kerneltune {
            s.tick();
        }
    }

    /// Drains a pending suspend-and-run request from the active screen, if
    /// any. Only the SSH Server Manager currently produces these (launching
    /// real `ssh`).
    fn take_pending_action(&mut self) -> Option<PendingAction> {
        self.easyssh.as_mut().and_then(|s| s.take_pending_action())
    }

    /// Bracketed-paste text arrives as one blob instead of individual key
    /// events, which matters for multi-line pastes: without bracketed
    /// paste, the terminal would deliver each line break as a real `Enter`
    /// keypress, which — on a single-line `Input` field — activates
    /// whatever button/action Enter means there instead of typing a
    /// newline. Normalizing line breaks to `", "` first means a pasted
    /// column of IPs behaves exactly like typing them comma-separated,
    /// which every "list of hosts" field already parses that way. Each
    /// character is then fed through the normal per-field key handling, so
    /// it lands exactly where typing would.
    fn handle_paste(&mut self, text: &str) {
        for c in normalize_paste(text).chars() {
            self.handle_key(KeyEvent::new(crossterm::event::KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    /// Terminal size as a `Rect`, the same shape every screen's `draw()`
    /// gets — mouse coordinates from crossterm are already relative to
    /// this, so screens can hit-test against it without any extra state
    /// threaded through from the render loop.
    fn term_area() -> Rect {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
        Rect::new(0, 0, cols, rows)
    }

    fn handle_mouse(&mut self, me: MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};
        match me.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let now = Instant::now();
                let is_repeat = self
                    .last_click
                    .is_some_and(|(x, y, t)| x == me.column && y == me.row && now.duration_since(t) < CLICK_DEBOUNCE);
                self.last_click = Some((me.column, me.row, now));
                if is_repeat {
                    return;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => return,
            _ => {}
        }

        let area = Self::term_area();
        match self.screen {
            Screen::Home => {
                if let Some(target) = self.home.handle_mouse(me, area) {
                    self.enter(target);
                }
            }
            Screen::EasySsh => {
                if let Some(s) = &mut self.easyssh {
                    s.handle_mouse(me, area);
                }
            }
            Screen::SshUser => {
                if let Some(s) = &mut self.sshuser {
                    s.handle_mouse(me, area);
                }
            }
            Screen::GoDaddy => {
                if let Some(s) = &mut self.godaddy {
                    s.handle_mouse(me, area);
                }
            }
            Screen::Mysql => {
                if let Some(s) = &mut self.mysql {
                    s.handle_mouse(me, area);
                }
            }
            Screen::Postgresql => {
                if let Some(s) = &mut self.postgresql {
                    s.handle_mouse(me, area);
                }
            }
            Screen::ClickHouse => {
                if let Some(s) = &mut self.clickhouse {
                    s.handle_mouse(me, area);
                }
            }
            Screen::Logs => {
                if let Some(s) = &mut self.logs {
                    s.handle_mouse(me, area);
                }
            }
            Screen::KernelTune => {
                if let Some(s) = &mut self.kerneltune {
                    s.handle_mouse(me, area);
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == crossterm::event::KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        // Global, works from any screen: with mouse capture off, the
        // terminal handles drags itself again, so a plain click-drag
        // (no modifier key needed) selects text natively and the
        // terminal's own copy shortcut (Ctrl+Shift+C, Cmd+C, right-click,
        // ...) copies it — none of that is something this app can
        // implement itself, since native selection only exists when we
        // *aren't* consuming mouse events.
        if key.code == crossterm::event::KeyCode::F(12) {
            self.mouse_enabled = !self.mouse_enabled;
            return;
        }

        match self.screen {
            Screen::Home => {
                if key.code == crossterm::event::KeyCode::Esc || key.code == crossterm::event::KeyCode::Char('q') {
                    self.should_quit = true;
                    return;
                }
                if let Some(target) = self.home.handle_key(key) {
                    self.enter(target);
                }
            }
            Screen::EasySsh => {
                let back = self.easyssh.as_mut().map(|s| s.handle_key(key)).unwrap_or(true);
                if back {
                    self.screen = Screen::Home;
                }
            }
            Screen::SshUser => {
                let back = self.sshuser.as_mut().map(|s| s.handle_key(key)).unwrap_or(true);
                if back {
                    self.screen = Screen::Home;
                }
            }
            Screen::GoDaddy => {
                let back = self.godaddy.as_mut().map(|s| s.handle_key(key)).unwrap_or(true);
                if back {
                    self.screen = Screen::Home;
                }
            }
            Screen::Mysql => {
                let back = self.mysql.as_mut().map(|s| s.handle_key(key)).unwrap_or(true);
                if back {
                    self.screen = Screen::Home;
                }
            }
            Screen::Postgresql => {
                let back = self.postgresql.as_mut().map(|s| s.handle_key(key)).unwrap_or(true);
                if back {
                    self.screen = Screen::Home;
                }
            }
            Screen::ClickHouse => {
                let back = self.clickhouse.as_mut().map(|s| s.handle_key(key)).unwrap_or(true);
                if back {
                    self.screen = Screen::Home;
                }
            }
            Screen::Logs => {
                let back = self.logs.as_mut().map(|s| s.handle_key(key)).unwrap_or(true);
                if back {
                    self.screen = Screen::Home;
                }
            }
            Screen::KernelTune => {
                let back = self.kerneltune.as_mut().map(|s| s.handle_key(key)).unwrap_or(true);
                if back {
                    self.screen = Screen::Home;
                }
            }
        }
    }

    fn draw(&self, f: &mut ratatui::Frame) {
        let area = f.area();
        f.render_widget(Block::default().style(Style::default().bg(BG)), area);
        match self.screen {
            Screen::Home => home::draw(f, &self.home, area),
            Screen::EasySsh => {
                if let Some(s) = &self.easyssh {
                    s.draw(f, area);
                }
            }
            Screen::SshUser => {
                if let Some(s) = &self.sshuser {
                    s.draw(f, area);
                }
            }
            Screen::GoDaddy => {
                if let Some(s) = &self.godaddy {
                    s.draw(f, area);
                }
            }
            Screen::Mysql => {
                if let Some(s) = &self.mysql {
                    s.draw(f, area);
                }
            }
            Screen::Postgresql => {
                if let Some(s) = &self.postgresql {
                    s.draw(f, area);
                }
            }
            Screen::ClickHouse => {
                if let Some(s) = &self.clickhouse {
                    s.draw(f, area);
                }
            }
            Screen::Logs => {
                if let Some(s) = &self.logs {
                    s.draw(f, area);
                }
            }
            Screen::KernelTune => {
                if let Some(s) = &self.kerneltune {
                    s.draw(f, area);
                }
            }
        }
    }
}

pub fn run() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Mouse capture starts off (see `App::mouse_enabled`) — `run_app`'s
    // first iteration reconciles it against `App::new()`'s default, so
    // it's deliberately not requested here.
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let res = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), DisableBracketedPaste, LeaveAlternateScreen, DisableMouseCaptureMinimal)?;
    terminal.show_cursor()?;
    res
}

fn run_app<B: ratatui::backend::Backend + io::Write>(terminal: &mut Terminal<B>, app: &mut App) -> io::Result<()> {
    let mut mouse_capture_active = false;
    loop {
        app.tick();
        terminal.draw(|f| app.draw(f))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => app.handle_key(key),
                Event::Paste(text) => app.handle_paste(&text),
                Event::Mouse(me) => app.handle_mouse(me),
                _ => {}
            }
        }

        if app.mouse_enabled != mouse_capture_active {
            mouse_capture_active = app.mouse_enabled;
            if mouse_capture_active {
                execute!(terminal.backend_mut(), EnableMouseCaptureMinimal)?;
            } else {
                execute!(terminal.backend_mut(), DisableMouseCaptureMinimal)?;
            }
        }

        if let Some(action) = app.take_pending_action() {
            run_pending_action(terminal, app, action)?;
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

/// Collapses a pasted block down to one line, joining what were separate
/// lines with `", "` (and dropping blank ones) so a column of pasted
/// values reads the same as a comma-separated list typed by hand.
fn normalize_paste(text: &str) -> String {
    text.split(['\n', '\r']).map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(", ")
}

/// Leaves TUI mode entirely — disables raw mode, drops the alternate
/// screen — runs `program` with the real terminal handed to it (so an
/// interactive `ssh` session behaves exactly like running it from a normal
/// shell), then restores the TUI and forces a full redraw.
fn run_pending_action<B: ratatui::backend::Backend + io::Write>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    action: PendingAction,
) -> io::Result<()> {
    match action {
        PendingAction::RunInteractive { program, args, alias } => {
            disable_raw_mode()?;
            execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCaptureMinimal)?;

            let status = std::process::Command::new(&program).args(&args).status();

            enable_raw_mode()?;
            execute!(terminal.backend_mut(), EnterAlternateScreen)?;
            if app.mouse_enabled {
                execute!(terminal.backend_mut(), EnableMouseCaptureMinimal)?;
            }
            terminal.clear()?;

            if let Some(s) = &mut app.easyssh {
                s.on_interactive_done(&alias, status);
            }
        }
    }
    Ok(())
}
