//! RAII terminal mode guard and shared TUI bootstrap restoring raw mode, alternate screen,
//! and mouse capture cleanly on normal exit or panic.

use std::io;
use std::sync::Arc;

use crossterm::cursor::Show;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        Show
    );
}

/// Restores terminal state when dropped or on panic.
pub(crate) struct TerminalGuard {
    prev_hook: Option<Arc<PanicHook>>,
}

impl TerminalGuard {
    pub(crate) fn enter() -> Self {
        let prev = Arc::new(std::panic::take_hook());
        let hook_prev = Arc::clone(&prev);
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            hook_prev(info);
        }));
        Self {
            prev_hook: Some(prev),
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
        if let Some(prev) = self.prev_hook.take() {
            let _ = std::panic::take_hook();
            match Arc::try_unwrap(prev) {
                Ok(original) => std::panic::set_hook(original),
                Err(shared) => std::panic::set_hook(Box::new(move |info| shared(info))),
            }
        }
    }
}

/// Enters raw mode, alternate screen, and mouse capture, returning a drop guard and terminal.
pub(crate) fn enter_tui() -> io::Result<(TerminalGuard, Terminal<CrosstermBackend<io::Stdout>>)> {
    let guard = TerminalGuard::enter();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok((guard, terminal))
}
