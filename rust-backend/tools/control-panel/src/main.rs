//! Control panel TUI for the rust-backend workspace.
//!
//! Starts/stops every binary in the workspace and streams its stdout+stderr
//! back live. Each binary registers itself via `shared::define_program!`, so
//! flag metadata (defaults, possible values, help text) flows directly from
//! the clap `#[derive(Parser)]` into the TUI.
//!
//! Run from the workspace root:
//!
//! ```bash
//! cargo run -p control-panel
//! ```

mod app;
mod flags;
mod input;
mod process;
mod programs;
mod ui;

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::app::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    let programs = programs::all();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = AppState::new(programs, tx);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_loop(&mut terminal, &mut app, &mut rx).await;

    // Clean shutdown of any running processes.
    for proc in &mut app.processes {
        let _ = proc.stop().await;
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut AppState,
    rx: &mut mpsc::UnboundedReceiver<process::ProcEvent>,
) -> Result<()> {
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));

    loop {
        terminal.draw(|f| ui::draw(f, app))?;
        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            maybe_evt = keys.next() => {
                if let Some(Ok(Event::Key(k))) = maybe_evt {
                    input::handle_key(app, k).await?;
                }
            }
            Some(proc_evt) = rx.recv() => {
                app.ingest(proc_evt);
                // Drain any other pending events to coalesce redraws.
                while let Ok(more) = rx.try_recv() {
                    app.ingest(more);
                }
            }
            _ = tick.tick() => {
                // Periodic redraw so live log output appears even without
                // user input.
            }
        }
    }
}
