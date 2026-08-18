use std::io::{self, stdout};
use std::panic;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::App;
use crate::session::MessageRole;

pub fn run(app: &mut App) -> io::Result<()> {
    install_panic_restore();
    let mut terminal = TerminalSession::enter()?;

    terminal.draw(|frame| render(frame, app))?;

    while !app.should_quit {
        let event = event::read()?;
        if handle_event(app, event) {
            terminal.draw(|frame| render(frame, app))?;
        }
    }

    Ok(())
}

fn handle_event(app: &mut App, event: Event) -> bool {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => handle_key(app, key),
        Event::Resize(_, _) => true,
        Event::Paste(text) => {
            for ch in text.chars() {
                app.input.insert(ch);
            }
            true
        }
        _ => false,
    }
}

fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') => {
                app.should_quit = true;
                return false;
            }
            KeyCode::Char('a') => app.input.move_home(),
            KeyCode::Char('e') => app.input.move_end(),
            _ => return false,
        }
        return true;
    }

    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => app.input.insert('\n'),
        KeyCode::Enter => app.submit(),
        KeyCode::Char(ch) => app.input.insert(ch),
        KeyCode::Backspace => app.input.backspace(),
        KeyCode::Delete => app.input.delete(),
        KeyCode::Left => app.input.move_left(),
        KeyCode::Right => app.input.move_right(),
        KeyCode::Home => app.input.move_home(),
        KeyCode::End => app.input.move_end(),
        KeyCode::PageUp => app.scroll_up(8),
        KeyCode::PageDown => app.scroll_down(8),
        KeyCode::Esc => app.input.clear(),
        _ => return false,
    }
    true
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let input_height = app.input.line_count().saturating_add(2).clamp(3, 8);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, rows[0]);
    render_messages(frame, rows[1], app);
    render_input(frame, rows[2], app);
    render_status(frame, rows[3], app);
}

fn render_header(frame: &mut Frame<'_>, area: Rect) {
    let header = Paragraph::new(Line::from(vec![
        " MatrixCode".bold(),
        "                         ".into(),
        "provider · account ".dim(),
    ]));
    frame.render_widget(header, area);
}

fn render_messages(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mut lines = Vec::with_capacity(app.messages.len().saturating_mul(3));
    for message in &app.messages {
        let label = match message.role {
            MessageRole::User => "You",
            MessageRole::Assistant => "MatrixCode",
            MessageRole::System => "MatrixCode",
        };
        lines.push(Line::styled(label, Style::default().add_modifier(Modifier::BOLD)));
        lines.extend(message.text.lines().map(Line::from));
        lines.push(Line::default());
    }

    let max_scroll = lines.len().saturating_sub(area.height as usize).min(u16::MAX as usize) as u16;
    let offset = max_scroll.saturating_sub(app.scroll.min(max_scroll));
    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((offset, 0));
    frame.render_widget(paragraph, area);
}

fn render_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let text = format!("> {}", app.input.display_with_cursor());
    let input = Paragraph::new(text)
        .block(Block::default().borders(Borders::TOP | Borders::BOTTOM))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, area);
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    frame.render_widget(
        Paragraph::new(app.status.as_str()).style(Style::default().add_modifier(Modifier::DIM)),
        area,
    );
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut output = stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen, EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(error);
        }

        let backend = CrosstermBackend::new(output);
        match Terminal::new(backend) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                restore_terminal();
                Err(error)
            }
        }
    }

    fn draw<F>(&mut self, draw: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame<'_>),
    {
        self.terminal.draw(draw).map(|_| ())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(stdout(), DisableBracketedPaste, LeaveAlternateScreen);
}

fn install_panic_restore() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

trait LineStyleExt<'a> {
    fn bold(self) -> ratatui::text::Span<'a>;
    fn dim(self) -> ratatui::text::Span<'a>;
}

impl<'a> LineStyleExt<'a> for &'a str {
    fn bold(self) -> ratatui::text::Span<'a> {
        ratatui::text::Span::styled(self, Style::default().add_modifier(Modifier::BOLD))
    }

    fn dim(self) -> ratatui::text::Span<'a> {
        ratatui::text::Span::styled(self, Style::default().add_modifier(Modifier::DIM))
    }
}
