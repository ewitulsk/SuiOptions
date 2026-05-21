//! Central UI state.
//!
//! Single-threaded mutation from the input/event loop in `main.rs`. The only
//! async surface is process I/O, which lands here via [`ProcEvent`] on a
//! tokio channel.

use std::collections::HashMap;

use anyhow::Result;
use shared::program_spec::ProgramSpec;
use tokio::sync::mpsc;

use crate::flags::{EnvVar, FlagValue, ProgramFlags};
use crate::process::{ProcEvent, ProcessHandle, Status};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Flags,
    Env,
    Logs,
    Docs,
}

impl Tab {
    pub fn all() -> &'static [Tab] {
        &[Tab::Flags, Tab::Env, Tab::Logs, Tab::Docs]
    }
    pub fn label(self) -> &'static str {
        match self {
            Tab::Flags => "Flags",
            Tab::Env => "Env",
            Tab::Logs => "Logs",
            Tab::Docs => "Docs",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    ProgramList,
    Content,
    EditingFlag,
    EditingEnvKey,
    EditingEnvValue,
    PickingSubcommand,
}

pub struct AppState {
    pub programs: Vec<ProgramSpec>,
    pub flags: Vec<ProgramFlags>,
    pub processes: Vec<ProcessHandle>,
    /// Index into `programs`.
    pub selected_program: usize,
    pub tab: Tab,
    pub focus: Focus,
    /// Index into the *visible* flag list (skipping headers).
    pub selected_flag: usize,
    /// Index into the per-program env-var list.
    pub selected_env: usize,
    /// Text buffer for the inline flag/env editor.
    pub edit_buffer: String,
    /// Which env var the editor is currently mutating (key or value depends
    /// on `focus`). `None` when not editing env.
    pub editing_env_idx: Option<usize>,
    /// Status-line message displayed at the bottom for ~1 redraw cycle.
    pub status_msg: Option<String>,
    /// Log scroll offset (lines from bottom). 0 means "follow tail".
    pub log_scroll: usize,
    pub should_quit: bool,
    pub events: mpsc::UnboundedSender<ProcEvent>,
    by_id: HashMap<String, usize>,
}

impl AppState {
    pub fn new(programs: Vec<ProgramSpec>, events: mpsc::UnboundedSender<ProcEvent>) -> Self {
        let flags = programs.iter().map(ProgramFlags::from_spec).collect();
        let processes = programs
            .iter()
            .map(|p| ProcessHandle::new(p.id))
            .collect();
        let by_id = programs
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id.to_string(), i))
            .collect();
        Self {
            programs,
            flags,
            processes,
            selected_program: 0,
            tab: Tab::Flags,
            focus: Focus::ProgramList,
            selected_flag: 0,
            selected_env: 0,
            edit_buffer: String::new(),
            editing_env_idx: None,
            status_msg: None,
            log_scroll: 0,
            should_quit: false,
            events,
            by_id,
        }
    }

    pub fn current_program(&self) -> &ProgramSpec {
        &self.programs[self.selected_program]
    }

    pub fn current_flags(&self) -> &ProgramFlags {
        &self.flags[self.selected_program]
    }

    pub fn current_flags_mut(&mut self) -> &mut ProgramFlags {
        &mut self.flags[self.selected_program]
    }

    pub fn current_process(&self) -> &ProcessHandle {
        &self.processes[self.selected_program]
    }

    pub fn select_next_program(&mut self) {
        if self.programs.is_empty() {
            return;
        }
        self.selected_program = (self.selected_program + 1) % self.programs.len();
        self.selected_flag = 0;
        self.selected_env = 0;
        self.log_scroll = 0;
    }

    pub fn select_prev_program(&mut self) {
        if self.programs.is_empty() {
            return;
        }
        if self.selected_program == 0 {
            self.selected_program = self.programs.len() - 1;
        } else {
            self.selected_program -= 1;
        }
        self.selected_flag = 0;
        self.selected_env = 0;
        self.log_scroll = 0;
    }

    pub fn cycle_tab(&mut self, dir: i32) {
        let n = Tab::all().len() as i32;
        let cur = Tab::all().iter().position(|t| *t == self.tab).unwrap_or(0) as i32;
        let next = ((cur + dir).rem_euclid(n)) as usize;
        self.tab = Tab::all()[next];
    }

    /// Spawn the currently-selected program if it isn't running.
    pub fn launch_current(&mut self) -> Result<()> {
        let idx = self.selected_program;
        if self.processes[idx].is_running() {
            self.set_status("already running");
            return Ok(());
        }
        let argv = self.flags[idx].build_argv();
        let envs = self.flags[idx].build_env();
        let spec = &self.programs[idx];
        let events = self.events.clone();
        self.processes[idx].spawn(spec.cargo_pkg, spec.working_dir, &argv, &envs, events)?;
        self.set_status("started");
        Ok(())
    }

    pub async fn stop_current(&mut self) -> Result<()> {
        let idx = self.selected_program;
        if !self.processes[idx].is_running() {
            self.set_status("not running");
            return Ok(());
        }
        self.processes[idx].stop().await?;
        self.set_status("stop signal sent");
        Ok(())
    }

    pub fn ingest(&mut self, evt: ProcEvent) {
        match evt {
            ProcEvent::Started { .. } | ProcEvent::Line { .. } => {}
            ProcEvent::Exited { program_id, code } => {
                if let Some(&idx) = self.by_id.get(&program_id) {
                    self.processes[idx].mark_exited(code);
                }
            }
        }
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_msg = Some(msg.into());
    }

    // -- flag editing --------------------------------------------------------

    /// Walk the visible flag list and find the FlagState at the current
    /// `selected_flag` index, skipping header rows.
    pub fn with_selected_flag<R>(
        &mut self,
        f: impl FnOnce(&mut crate::flags::FlagState) -> R,
    ) -> Option<R> {
        let target = self.selected_flag;
        let rows = self.current_flags_mut().visible_flags_mut();
        let mut data_idx = 0usize;
        for row in rows {
            if let crate::flags::FlagRow::Flag(state) = row {
                if data_idx == target {
                    return Some(f(state));
                }
                data_idx += 1;
            }
        }
        None
    }

    pub fn visible_flag_count(&self) -> usize {
        // Recompute via spec to avoid the borrow-mut requirement.
        let flags = self.current_flags();
        let mut n = flags.global.len();
        if let Some(idx) = flags.selected_subcommand {
            if let Some(sub) = flags.subcommands.get(idx) {
                n += sub.flags.len();
            }
        }
        n
    }

    pub fn begin_edit_flag(&mut self) {
        let cur_text = self
            .with_selected_flag(|f| match &f.value {
                FlagValue::Text(s) => Some(s.clone()),
                _ => None,
            })
            .flatten();
        if let Some(s) = cur_text {
            self.edit_buffer = s;
            self.focus = Focus::EditingFlag;
        } else {
            // Bool flag: toggle directly.
            self.with_selected_flag(|f| f.toggle());
            self.set_status("toggled");
        }
    }

    pub fn commit_edit_flag(&mut self) {
        let new = self.edit_buffer.clone();
        self.with_selected_flag(|f| f.set_text(new));
        self.focus = Focus::Content;
        self.edit_buffer.clear();
        self.set_status("flag updated");
    }

    pub fn cancel_edit_flag(&mut self) {
        self.edit_buffer.clear();
        self.focus = Focus::Content;
    }

    // -- env editing ---------------------------------------------------------

    pub fn env_count(&self) -> usize {
        self.current_flags().env.len()
    }

    fn clamp_selected_env(&mut self) {
        let n = self.env_count();
        if n == 0 {
            self.selected_env = 0;
        } else if self.selected_env >= n {
            self.selected_env = n - 1;
        }
    }

    pub fn select_next_env(&mut self) {
        let n = self.env_count();
        if n > 0 {
            self.selected_env = (self.selected_env + 1) % n;
        }
    }

    pub fn select_prev_env(&mut self) {
        let n = self.env_count();
        if n > 0 {
            self.selected_env = if self.selected_env == 0 {
                n - 1
            } else {
                self.selected_env - 1
            };
        }
    }

    /// Open the inline editor on the selected env var's value.
    pub fn begin_edit_env_value(&mut self) {
        let idx = self.selected_env;
        if let Some(e) = self.current_flags().env.get(idx) {
            self.edit_buffer = e.value.clone();
            self.editing_env_idx = Some(idx);
            self.focus = Focus::EditingEnvValue;
        }
    }

    /// Open the inline editor on the selected env var's key (rename).
    pub fn begin_edit_env_key(&mut self) {
        let idx = self.selected_env;
        if let Some(e) = self.current_flags().env.get(idx) {
            self.edit_buffer = e.key.clone();
            self.editing_env_idx = Some(idx);
            self.focus = Focus::EditingEnvKey;
        }
    }

    /// Append a blank env var and open the key editor on it.
    pub fn add_env_var(&mut self) {
        let flags = self.current_flags_mut();
        flags.env.push(EnvVar {
            key: String::new(),
            value: String::new(),
        });
        self.selected_env = self.current_flags().env.len() - 1;
        self.begin_edit_env_key();
    }

    pub fn delete_env_var(&mut self) {
        let idx = self.selected_env;
        let flags = self.current_flags_mut();
        if idx < flags.env.len() {
            flags.env.remove(idx);
            self.clamp_selected_env();
            self.set_status("env var removed");
        }
    }

    pub fn commit_edit_env(&mut self) {
        let new = std::mem::take(&mut self.edit_buffer);
        let focus = self.focus;
        if let Some(idx) = self.editing_env_idx {
            let flags = self.current_flags_mut();
            if let Some(e) = flags.env.get_mut(idx) {
                match focus {
                    Focus::EditingEnvKey => e.key = new,
                    Focus::EditingEnvValue => e.value = new,
                    _ => {}
                }
            }
        }
        self.editing_env_idx = None;
        self.focus = Focus::Content;
        self.set_status("env updated");
    }

    pub fn cancel_edit_env(&mut self) {
        self.edit_buffer.clear();
        self.editing_env_idx = None;
        self.focus = Focus::Content;
    }
}

pub fn status_label(status: &Status) -> (&'static str, ratatui::style::Color) {
    use ratatui::style::Color;
    match status {
        Status::Stopped => ("STOPPED", Color::DarkGray),
        Status::Running { .. } => ("RUNNING", Color::Green),
        Status::Exited { code: Some(0) } => ("DONE", Color::Cyan),
        Status::Exited { .. } => ("CRASHED", Color::Red),
    }
}
