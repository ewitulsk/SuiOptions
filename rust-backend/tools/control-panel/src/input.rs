//! Key handling. Maps crossterm key events onto AppState mutations.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::{AppState, Focus, Tab};

pub async fn handle_key(app: &mut AppState, key: KeyEvent) -> Result<()> {
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }
    // Clear last status on any keypress so it acts as transient feedback.
    app.status_msg = None;

    match app.focus {
        Focus::EditingFlag => handle_edit_key(app, key),
        Focus::EditingEnvKey | Focus::EditingEnvValue => handle_env_edit_key(app, key),
        Focus::PickingSubcommand => handle_picker_key(app, key),
        Focus::ProgramList | Focus::Content => handle_main_key(app, key).await?,
    }
    Ok(())
}

async fn handle_main_key(app: &mut AppState, key: KeyEvent) -> Result<()> {
    // Global keys.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return Ok(());
    }
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('r') => app.launch_current()?,
        KeyCode::Char('s') => app.stop_current().await?,
        KeyCode::Char('R') => {
            // Restart: stop then re-launch.
            app.stop_current().await?;
            app.launch_current()?;
        }
        KeyCode::Tab => app.cycle_tab(1),
        KeyCode::BackTab => app.cycle_tab(-1),
        KeyCode::Up => app.select_prev_program(),
        KeyCode::Down => app.select_next_program(),
        KeyCode::Right => app.focus = Focus::Content,
        KeyCode::Left => app.focus = Focus::ProgramList,
        _ => match app.tab {
            Tab::Flags => handle_flags_key(app, key),
            Tab::Env => handle_env_key(app, key),
            Tab::Logs => handle_logs_key(app, key),
            Tab::Docs => {}
        },
    }
    Ok(())
}

fn handle_flags_key(app: &mut AppState, key: KeyEvent) {
    match key.code {
        KeyCode::Char('j') => {
            let n = app.visible_flag_count();
            if n > 0 {
                app.selected_flag = (app.selected_flag + 1) % n;
            }
        }
        KeyCode::Char('k') => {
            let n = app.visible_flag_count();
            if n > 0 {
                if app.selected_flag == 0 {
                    app.selected_flag = n - 1;
                } else {
                    app.selected_flag -= 1;
                }
            }
        }
        KeyCode::Char('e') | KeyCode::Enter => app.begin_edit_flag(),
        KeyCode::Char(' ') => {
            // Space toggles bool / cycles enum.
            app.with_selected_flag(|f| {
                if !f.spec.possible_values.is_empty() {
                    f.cycle_possible_values(1);
                } else {
                    f.toggle();
                }
            });
        }
        KeyCode::Char('R') => {
            app.with_selected_flag(|f| f.revert());
            app.set_status("flag reverted");
        }
        KeyCode::Char('c') => {
            if !app.current_flags().subcommands.is_empty() {
                app.focus = Focus::PickingSubcommand;
            }
        }
        _ => {}
    }
}

fn handle_logs_key(app: &mut AppState, key: KeyEvent) {
    match key.code {
        KeyCode::Char('j') | KeyCode::Char(' ') => {
            app.log_scroll = app.log_scroll.saturating_sub(1)
        }
        KeyCode::Char('k') => app.log_scroll = app.log_scroll.saturating_add(1),
        KeyCode::Char('g') => app.log_scroll = usize::MAX, // top
        KeyCode::Char('G') => app.log_scroll = 0,          // bottom
        _ => {}
    }
}

fn handle_edit_key(app: &mut AppState, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.cancel_edit_flag(),
        KeyCode::Enter => app.commit_edit_flag(),
        KeyCode::Backspace => {
            app.edit_buffer.pop();
        }
        KeyCode::Char(c) => app.edit_buffer.push(c),
        _ => {}
    }
}

fn handle_env_key(app: &mut AppState, key: KeyEvent) {
    match key.code {
        KeyCode::Char('j') => app.select_next_env(),
        KeyCode::Char('k') => app.select_prev_env(),
        KeyCode::Char('e') | KeyCode::Enter => app.begin_edit_env_value(),
        KeyCode::Char('K') => app.begin_edit_env_key(),
        KeyCode::Char('a') => app.add_env_var(),
        KeyCode::Char('d') => app.delete_env_var(),
        _ => {}
    }
}

fn handle_env_edit_key(app: &mut AppState, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.cancel_edit_env(),
        KeyCode::Enter => app.commit_edit_env(),
        KeyCode::Backspace => {
            app.edit_buffer.pop();
        }
        KeyCode::Char(c) => app.edit_buffer.push(c),
        _ => {}
    }
}

fn handle_picker_key(app: &mut AppState, key: KeyEvent) {
    let n = app.current_flags().subcommands.len();
    match key.code {
        KeyCode::Esc => app.focus = Focus::Content,
        KeyCode::Enter => app.focus = Focus::Content,
        KeyCode::Up | KeyCode::Char('k') => {
            let flags = app.current_flags_mut();
            let cur = flags.selected_subcommand.unwrap_or(0);
            flags.selected_subcommand = Some(if cur == 0 { n - 1 } else { cur - 1 });
            app.selected_flag = 0;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let flags = app.current_flags_mut();
            let cur = flags.selected_subcommand.unwrap_or(0);
            flags.selected_subcommand = Some((cur + 1) % n);
            app.selected_flag = 0;
        }
        _ => {}
    }
}
