//! The terminal the view draws into, and giving it back.
//!
//! A view that leaves the terminal in the alternate screen, or in raw
//! mode, has broken the operator's shell. Two rules hold here: the
//! screen is entered once and restored once, and the restore happens
//! however the view ends — a normal exit, an error, or a panic in a
//! draw function.

use std::io::{self, Stdout};

use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    crossterm::{
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
};

/// A terminal the view can draw into.
pub trait Screen {
    /// Put the terminal into the mode the view needs.
    fn enter(&mut self) -> io::Result<()>;
    /// Put it back, whatever happened.
    fn restore(&mut self) -> io::Result<()>;
    /// Draw one frame.
    fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()>;
}

/// The real terminal: the alternate screen, in raw mode.
pub struct CrosstermScreen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl CrosstermScreen {
    /// Build the screen. Nothing is changed yet: the terminal is only
    /// taken over when `enter` is called.
    pub fn new() -> io::Result<Self> {
        let terminal =
            Terminal::new(CrosstermBackend::new(io::stdout())).map_err(io::Error::other)?;
        Ok(Self { terminal })
    }
}

impl Screen for CrosstermScreen {
    fn enter(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        execute!(self.terminal.backend_mut(), EnterAlternateScreen)?;
        self.terminal.hide_cursor()?;
        Ok(())
    }

    fn restore(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        Ok(())
    }

    fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
        self.terminal
            .draw(render)
            .map(|_| ())
            .map_err(io::Error::other)
    }
}

/// Gives the screen back when it is dropped, however the view ends.
///
/// The restore lives in `Drop` on purpose: a panic that unwinds
/// through the view, and a task that is dropped when the runtime
/// shuts it down, both run the destructors on the way out. A view that
/// ends in either of those ways still leaves the operator with a
/// terminal that echoes.
pub struct Guard<S: Screen> {
    screen: Option<S>,
}

impl<S: Screen> Guard<S> {
    /// Take responsibility for a screen.
    pub fn new(screen: S) -> Self {
        Self {
            screen: Some(screen),
        }
    }

    /// Give the screen back now.
    pub fn release(&mut self) -> io::Result<()> {
        let Some(screen) = self.screen.as_mut() else {
            return Ok(());
        };
        let result = screen.restore();
        if result.is_ok() {
            // The screen is back, so the guard has nothing left to do.
            // A failed restore keeps it, because the drop below tries
            // again: a terminal left in the alternate screen is worse
            // than a second attempt.
            self.screen = None;
        }
        result
    }

    /// Draw one frame.
    pub fn draw(&mut self, render: impl FnOnce(&mut Frame)) -> io::Result<()> {
        match self.screen.as_mut() {
            Some(screen) => screen.draw(render),
            None => Ok(()),
        }
    }
}

impl<S: Screen> Drop for Guard<S> {
    fn drop(&mut self) {
        if let Some(screen) = self.screen.as_mut() {
            let _ = screen.restore();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A screen that records what was done to it.
    struct Fake {
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Screen for Fake {
        fn enter(&mut self) -> io::Result<()> {
            self.calls.lock().unwrap().push("enter");
            Ok(())
        }

        fn restore(&mut self) -> io::Result<()> {
            self.calls.lock().unwrap().push("restore");
            Ok(())
        }

        fn draw(&mut self, _render: impl FnOnce(&mut Frame)) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_panic_still_gives_the_terminal_back() {
        // The defect this protects against: a panic in a draw function
        // leaves the operator in the alternate screen with no echo, and
        // the shell they came from is unusable until they reset it.
        let calls = Arc::new(Mutex::new(Vec::new()));
        let screen = Fake {
            calls: calls.clone(),
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = Guard::new(screen);
            panic!("the draw function fell over");
        }));
        assert!(outcome.is_err(), "the panic did not happen");
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["restore"],
            "the terminal was not given back after the panic"
        );
    }

    #[test]
    fn the_screen_is_given_back_exactly_once() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let screen = Fake {
            calls: calls.clone(),
        };
        {
            let mut guard = Guard::new(screen);
            guard.release().expect("the restore works");
        }
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["restore"],
            "the screen was restored more than once"
        );
    }
}
