use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use regex::Regex;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Severity {
    Error,
    Warning,
    Note,
    Other,
}

impl Severity {
    fn color(&self) -> Color {
        match self {
            Severity::Error => Color::Red,
            Severity::Warning => Color::Yellow,
            Severity::Note => Color::Cyan,
            Severity::Other => Color::Gray,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Severity::Error => "ERR",
            Severity::Warning => "WRN",
            Severity::Note => "NTE",
            Severity::Other => "   ",
        }
    }
}

#[derive(Debug, Clone)]
struct Diagnostic {
    file: String,
    line: u32,
    col: u32,
    severity: Severity,
    message: String,
    context: Vec<String>,
}

impl Diagnostic {
    fn location(&self) -> String {
        format!("{}:{}:{}", self.file, self.line, self.col)
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    gcc_re: Regex,
    msvc_re: Regex,
    ansi_re: Regex,
}

impl Parser {
    fn new() -> Self {
        Parser {
            gcc_re: Regex::new(
                r"^(?P<f>[^:\n\r]+):(?P<l>\d+):(?P<c>\d+):\s+(?P<s>error|warning|note|fatal error):\s+(?P<m>.+)$",
            ).unwrap(),
            msvc_re: Regex::new(
                r"^(?P<f>[^()\n\r]+)\((?P<l>\d+),(?P<c>\d+)\):\s+(?P<s>error|warning)\s+\w+:\s+(?P<m>.+)$",
            ).unwrap(),
            ansi_re: Regex::new(r"\x1b\[[0-9;]*[mGKHF]").unwrap(),
        }
    }

    fn strip_ansi<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str> {
        self.ansi_re.replace_all(s, "")
    }

    fn parse_line(&self, raw: &str) -> Option<Diagnostic> {
        let clean = self.strip_ansi(raw);
        let clean = clean.trim_end();
        let cap = self.gcc_re.captures(clean).or_else(|| self.msvc_re.captures(clean))?;
        let severity = match cap.name("s").map(|m| m.as_str()) {
            Some("error") | Some("fatal error") => Severity::Error,
            Some("warning") => Severity::Warning,
            Some("note") => Severity::Note,
            _ => Severity::Other,
        };
        Some(Diagnostic {
            file: cap["f"].to_string(),
            line: cap["l"].parse().unwrap_or(1),
            col: cap["c"].parse().unwrap_or(1),
            severity,
            message: cap["m"].trim().to_string(),
            context: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Build runner
// ---------------------------------------------------------------------------

fn process_stream_lines(
    lines: impl Iterator<Item = io::Result<String>>,
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
    raw_lines: Arc<Mutex<Vec<String>>>,
    log_file: Arc<Mutex<std::fs::File>>,
) {
    let parser = Parser::new();
    let mut last_diag_index: Option<usize> = None;

    for line_result in lines {
        let Ok(line) = line_result else { break };
        {
            let mut f = log_file.lock().unwrap();
            let _ = writeln!(f, "{}", line);
        }
        {
            let mut r = raw_lines.lock().unwrap();
            r.push(line.clone());
        }
        if let Some(diag) = parser.parse_line(&line) {
            last_diag_index = Some({
                let mut d = diagnostics.lock().unwrap();
                d.push(diag);
                d.len() - 1
            });
        } else {
            let clean = parser.strip_ansi(&line);
            let trimmed = clean.trim();
            if !trimmed.is_empty() {
                if let Some(idx) = last_diag_index {
                    let mut d = diagnostics.lock().unwrap();
                    if let Some(diag) = d.get_mut(idx) {
                        if diag.context.len() < 8 {
                            diag.context.push(trimmed.to_string());
                        }
                    }
                }
            }
        }
    }
}

fn run_build(
    build_cmd: String,
    build_dir: String,
    log_path: String,
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
    build_done: Arc<Mutex<bool>>,
    build_success: Arc<Mutex<bool>>,
    raw_lines: Arc<Mutex<Vec<String>>>,
) {
    let log_file = Arc::new(Mutex::new(
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .unwrap_or_else(|_| {
                OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open("/tmp/cpphxbuilder.log")
                    .expect("Cannot open log file")
            }),
    ));

    let shell_cmd = format!("cd {} && {}", build_dir, build_cmd);

    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(&shell_cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let mut r = raw_lines.lock().unwrap();
            r.push(format!("ERROR: Failed to spawn build: {}", e));
            *build_done.lock().unwrap() = true;
            return;
        }
    };

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let d1 = Arc::clone(&diagnostics);
    let r1 = Arc::clone(&raw_lines);
    let l1 = Arc::clone(&log_file);
    let t1 = thread::spawn(move || {
        process_stream_lines(BufReader::new(stdout).lines(), d1, r1, l1);
    });

    let d2 = Arc::clone(&diagnostics);
    let r2 = Arc::clone(&raw_lines);
    let l2 = Arc::clone(&log_file);
    let t2 = thread::spawn(move || {
        process_stream_lines(BufReader::new(stderr).lines(), d2, r2, l2);
    });

    t1.join().ok();
    t2.join().ok();

    let status = child.wait().expect("Failed to wait on child");
    *build_success.lock().unwrap() = status.success();
    *build_done.lock().unwrap() = true;
}

// ---------------------------------------------------------------------------
// TUI state
// ---------------------------------------------------------------------------

enum View { Diagnostics, Log }

struct App {
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
    raw_lines: Arc<Mutex<Vec<String>>>,
    build_done: Arc<Mutex<bool>>,
    build_success: Arc<Mutex<bool>>,
    list_state: ListState,
    log_scroll: u16,
    view: View,
    errors_only: bool,
}

impl App {
    fn new(
        diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
        raw_lines: Arc<Mutex<Vec<String>>>,
        build_done: Arc<Mutex<bool>>,
        build_success: Arc<Mutex<bool>>,
    ) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        App { diagnostics, raw_lines, build_done, build_success,
              list_state, log_scroll: 0, view: View::Diagnostics, errors_only: false }
    }

    fn visible_diagnostics(&self) -> Vec<Diagnostic> {
        let diags = self.diagnostics.lock().unwrap();
        if self.errors_only {
            diags.iter().filter(|d| d.severity == Severity::Error).cloned().collect()
        } else {
            diags.clone()
        }
    }

    fn selected_diagnostic(&self) -> Option<Diagnostic> {
        let visible = self.visible_diagnostics();
        self.list_state.selected().and_then(|i| visible.get(i).cloned())
    }

    fn move_up(&mut self) {
        let len = self.visible_diagnostics().len();
        if len == 0 { return; }
        let i = self.list_state.selected().unwrap_or(0);
        self.list_state.select(Some(if i == 0 { len - 1 } else { i - 1 }));
    }

    fn move_down(&mut self) {
        let len = self.visible_diagnostics().len();
        if len == 0 { return; }
        let i = self.list_state.selected().unwrap_or(0);
        self.list_state.select(Some((i + 1) % len));
    }

    fn page_up(&mut self, n: usize) {
        let i = self.list_state.selected().unwrap_or(0);
        self.list_state.select(Some(i.saturating_sub(n)));
    }

    fn page_down(&mut self, n: usize) {
        let len = self.visible_diagnostics().len();
        if len == 0 { return; }
        let i = self.list_state.selected().unwrap_or(0);
        self.list_state.select(Some((i + n).min(len - 1)));
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_ui(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.size();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(7), Constraint::Length(1)])
        .split(area);

    match app.view {
        View::Diagnostics => render_diagnostics(f, app, chunks[0]),
        View::Log => render_log(f, app, chunks[0]),
    }
    render_detail(f, app, chunks[1]);
    render_statusbar(f, app, chunks[2]);
}

fn shorten_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len { return path.to_string(); }
    let keep = max_len.saturating_sub(1);
    format!("…{}", &path[path.len().saturating_sub(keep)..])
}

fn render_diagnostics(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let visible = app.visible_diagnostics();
    let done = *app.build_done.lock().unwrap();

    let title = if done {
        let success = *app.build_success.lock().unwrap();
        let n_err = visible.iter().filter(|d| d.severity == Severity::Error).count();
        let n_warn = visible.iter().filter(|d| d.severity == Severity::Warning).count();
        format!(" Diagnostics  {} errors  {} warnings  — {} ",
            n_err, n_warn,
            if success { "✓ BUILD OK" } else { "✗ BUILD FAILED" })
    } else {
        format!(" Building…  {} diagnostics so far ", visible.len())
    };

    let max_file = (area.width as usize).saturating_sub(25).min(48).max(16);

    let items: Vec<ListItem> = visible.iter().map(|d| {
        let sev_style = Style::default().fg(d.severity.color()).add_modifier(Modifier::BOLD);
        let line = Line::from(vec![
            Span::styled(format!(" {} ", d.severity.label()), sev_style),
            Span::styled(
                format!("{:<width$}", shorten_path(&d.file, max_file), width = max_file),
                Style::default().fg(Color::Blue),
            ),
            Span::styled(
                format!(" :{:<4} ", d.line),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(d.message.clone(), Style::default().fg(Color::White)),
        ]);
        ListItem::new(line)
    }).collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title)
            .title_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)))
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_log(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let lines = app.raw_lines.lock().unwrap();
    let total = lines.len() as u16;
    let visible_height = area.height.saturating_sub(2);
    // auto-scroll to bottom during build
    let done = *app.build_done.lock().unwrap();
    let scroll = if !done {
        total.saturating_sub(visible_height)
    } else {
        app.log_scroll.min(total.saturating_sub(visible_height))
    };
    if !done { app.log_scroll = scroll; }

    let text: Vec<Line> = lines.iter()
        .skip(scroll as usize)
        .take(visible_height as usize)
        .map(|l| Line::from(Span::raw(l.clone())))
        .collect();

    let para = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL)
            .title(format!(" Build Output ({}/{} lines) ", scroll + visible_height, total))
            .title_style(Style::default().fg(Color::Cyan)))
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn render_detail(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let text = if let Some(diag) = app.selected_diagnostic() {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("  Location: ", Style::default().fg(Color::DarkGray)),
                Span::styled(diag.location(),
                    Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                Span::styled(format!("  [{}] ", diag.severity.label()),
                    Style::default().fg(diag.severity.color()).add_modifier(Modifier::BOLD)),
                Span::styled(diag.message.clone(), Style::default().fg(Color::White)),
            ]),
        ];
        for ctx in &diag.context {
            lines.push(Line::from(Span::styled(
                format!("  {}", ctx), Style::default().fg(Color::DarkGray))));
        }
        lines
    } else {
        vec![Line::from(Span::styled(" No diagnostics selected",
            Style::default().fg(Color::DarkGray)))]
    };

    let para = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(" Detail ")
            .title_style(Style::default().fg(Color::Cyan)))
        .wrap(Wrap { trim: true });
    f.render_widget(para, area);
}

fn render_statusbar(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let building = !*app.build_done.lock().unwrap();
    let view_hint = match app.view { View::Diagnostics => "Tab:log", View::Log => "Tab:diag" };
    let filter_hint = if app.errors_only { "f:show all" } else { "f:errors only" };
    let text = if building {
        format!("  Building…   {}  {}  q:quit", view_hint, filter_hint)
    } else {
        format!("  ↑/↓/j/k:nav   Enter:open in hx   r:rebuild   {}   {}   q:quit",
            view_hint, filter_hint)
    };
    let para = Paragraph::new(Line::from(Span::styled(text,
        Style::default().fg(Color::Black).bg(Color::DarkGray))));
    f.render_widget(para, area);
}

// ---------------------------------------------------------------------------
// Open in Helix
// ---------------------------------------------------------------------------

fn open_in_helix(diag: &Diagnostic) {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);

    let location = format!("{}:{}:{}", diag.file, diag.line, diag.col);
    let _ = Command::new("hx").arg(&location).status();

    let _ = enable_raw_mode();
    let _ = execute!(stdout, EnterAlternateScreen, EnableMouseCapture);
}

// ---------------------------------------------------------------------------
// Start build (returns shared state arcs)
// ---------------------------------------------------------------------------

fn start_build(build_cmd: &str, build_dir: &str, log_path: &str) -> (
    Arc<Mutex<Vec<Diagnostic>>>,
    Arc<Mutex<Vec<String>>>,
    Arc<Mutex<bool>>,
    Arc<Mutex<bool>>,
) {
    let diagnostics: Arc<Mutex<Vec<Diagnostic>>> = Arc::new(Mutex::new(Vec::new()));
    let raw_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let build_done: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let build_success: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

    let (d, r, done, succ) = (
        Arc::clone(&diagnostics), Arc::clone(&raw_lines),
        Arc::clone(&build_done), Arc::clone(&build_success),
    );
    let cmd = build_cmd.to_string();
    let dir = build_dir.to_string();
    let log = log_path.to_string();

    thread::spawn(move || run_build(cmd, dir, log, d, done, succ, r));

    (diagnostics, raw_lines, build_done, build_success)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> io::Result<()> {
    let build_dir = env::var("CPPHX_BUILD_DIR").unwrap_or_else(|_| "./build".to_string());
    let build_cmd = env::var("CPPHX_BUILD_CMD")
        .unwrap_or_else(|_| "cmake --build . -- -j 64".to_string());
    let log_path = env::var("CPPHX_LOG_PATH")
        .unwrap_or_else(|_| "cpphxbuilder.log".to_string());

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (diagnostics, raw_lines, build_done, build_success) =
        start_build(&build_cmd, &build_dir, &log_path);

    let mut app = App::new(
        Arc::clone(&diagnostics), Arc::clone(&raw_lines),
        Arc::clone(&build_done), Arc::clone(&build_success),
    );

    let mut needs_rebuild = false;
    let tick = std::time::Duration::from_millis(80);

    loop {
        terminal.draw(|f| render_ui(f, &mut app))?;

        if needs_rebuild {
            let (d, r, done, succ) = start_build(&build_cmd, &build_dir, &log_path);
            app.diagnostics = d;
            app.raw_lines = r;
            app.build_done = done;
            app.build_success = succ;
            app.list_state.select(Some(0));
            app.log_scroll = 0;
            needs_rebuild = false;
        }

        if !event::poll(tick)? { continue; }

        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('q') | KeyCode::Char('c')
                    if key.code == KeyCode::Char('q')
                    || key.modifiers.contains(KeyModifiers::CONTROL) => break,

                KeyCode::Up | KeyCode::Char('k') => app.move_up(),
                KeyCode::Down | KeyCode::Char('j') => app.move_down(),

                KeyCode::PageUp => {
                    let h = terminal.size()?.height as usize;
                    match app.view {
                        View::Diagnostics => app.page_up(h.saturating_sub(12)),
                        View::Log => { app.log_scroll = app.log_scroll.saturating_sub(h as u16); }
                    }
                }
                KeyCode::PageDown => {
                    let h = terminal.size()?.height as usize;
                    match app.view {
                        View::Diagnostics => app.page_down(h.saturating_sub(12)),
                        View::Log => { app.log_scroll = app.log_scroll.saturating_add(h as u16); }
                    }
                }
                KeyCode::Home => {
                    match app.view {
                        View::Diagnostics => { app.list_state.select(Some(0)); }
                        View::Log => { app.log_scroll = 0; }
                    }
                }
                KeyCode::End => {
                    match app.view {
                        View::Diagnostics => {
                            let len = app.visible_diagnostics().len();
                            if len > 0 { app.list_state.select(Some(len - 1)); }
                        }
                        View::Log => {
                            let total = app.raw_lines.lock().unwrap().len() as u16;
                            app.log_scroll = total;
                        }
                    }
                }

                KeyCode::Enter => {
                    if let Some(diag) = app.selected_diagnostic() {
                        terminal.clear()?;
                        open_in_helix(&diag);
                        terminal.clear()?;
                    }
                }

                KeyCode::Char('r') => { needs_rebuild = true; }

                KeyCode::Tab => {
                    app.view = match app.view {
                        View::Diagnostics => {
                            let total = app.raw_lines.lock().unwrap().len() as u16;
                            app.log_scroll = total;
                            View::Log
                        }
                        View::Log => View::Diagnostics,
                    };
                }

                KeyCode::Char('f') => {
                    app.errors_only = !app.errors_only;
                    let len = app.visible_diagnostics().len();
                    app.list_state.select(if len > 0 { Some(0) } else { None });
                }

                _ => {}
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}
