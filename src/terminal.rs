use std::{
    io::{self, Stdout},
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyEvent, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{Result, app::App};

type Tui = Terminal<CrosstermBackend<Stdout>>;

struct TerminalGuard {
    terminal: Tui,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self {
            terminal: Terminal::new(CrosstermBackend::new(stdout))?,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

pub async fn run(mut app: App) -> Result<()> {
    let mut terminal = TerminalGuard::enter()?;
    while !app.should_quit() {
        app.tick().await;
        terminal.terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(80))?
            && let Event::Key(key) = event::read()?
            && should_dispatch_key(&key)
        {
            app.handle_key(key);
        }
    }
    app.cancel_active_job();
    Ok(())
}

fn should_dispatch_key(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
}

#[cfg(test)]
mod tests {
    use super::should_dispatch_key;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    #[test]
    fn release_events_are_not_dispatched() {
        let press = KeyEvent::new_with_kind(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Press);
        let repeat = KeyEvent::new_with_kind(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Repeat);
        let release =
            KeyEvent::new_with_kind(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Release);

        assert!(should_dispatch_key(&press));
        assert!(should_dispatch_key(&repeat));
        assert!(!should_dispatch_key(&release));
    }
}
