//! Per-program flag state.
//!
//! Initial values come from each clap arg's `default_value`. The user can
//! override them in the TUI. When the program is launched, every overridden
//! flag becomes a CLI argument; non-overridden flags are omitted so the
//! binary picks its own defaults (which match what's stored here, by
//! construction).

use shared::program_spec::{ArgKind, ArgSpec, ProgramSpec, SubcommandSpec};

#[derive(Debug, Clone)]
pub enum FlagValue {
    /// `Value`-kind arg. Empty string == "not overridden".
    Text(String),
    /// `Flag`-kind arg (boolean toggle).
    Bool(bool),
}

#[derive(Debug, Clone)]
pub struct FlagState {
    pub spec: ArgSpec,
    pub value: FlagValue,
    /// True if the user has edited the value away from the clap default.
    pub overridden: bool,
}

impl FlagState {
    fn new(spec: ArgSpec) -> Self {
        let value = match spec.kind {
            ArgKind::Flag => FlagValue::Bool(false),
            ArgKind::Value | ArgKind::Multi => {
                FlagValue::Text(spec.default.clone().unwrap_or_default())
            }
        };
        Self {
            spec,
            value,
            overridden: false,
        }
    }

    #[allow(dead_code)]
    pub fn display_value(&self) -> String {
        match &self.value {
            FlagValue::Bool(b) => {
                if *b {
                    "on".to_string()
                } else {
                    "off".to_string()
                }
            }
            FlagValue::Text(s) => {
                if s.is_empty() {
                    "(empty)".to_string()
                } else {
                    s.clone()
                }
            }
        }
    }

    pub fn cycle_possible_values(&mut self, direction: i32) {
        if let FlagValue::Text(s) = &self.value {
            if self.spec.possible_values.is_empty() {
                return;
            }
            let cur = self
                .spec
                .possible_values
                .iter()
                .position(|p| p == s)
                .unwrap_or(0) as i32;
            let n = self.spec.possible_values.len() as i32;
            let next = ((cur + direction).rem_euclid(n)) as usize;
            self.value = FlagValue::Text(self.spec.possible_values[next].clone());
            self.overridden = true;
        }
    }

    pub fn toggle(&mut self) {
        if let FlagValue::Bool(b) = &mut self.value {
            *b = !*b;
            self.overridden = true;
        }
    }

    pub fn set_text(&mut self, s: String) {
        if matches!(self.value, FlagValue::Text(_)) {
            self.value = FlagValue::Text(s);
            self.overridden = true;
        }
    }

    pub fn revert(&mut self) {
        *self = FlagState::new(self.spec.clone());
    }
}

/// One environment variable. Empty `key` is treated as "not set" so the
/// add-then-edit flow has somewhere to live before the user types a name.
#[derive(Debug, Clone)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

/// All editable state for one program. If the program has subcommands, the
/// TUI also tracks which subcommand is selected and that subcommand's flags.
#[derive(Debug, Clone)]
pub struct ProgramFlags {
    pub global: Vec<FlagState>,
    pub subcommands: Vec<SubcommandFlags>,
    /// Index into `subcommands`, or `None` if the program has no
    /// subcommands. If `Some`, that subcommand's flags are appended after
    /// the subcommand name when building the argv.
    pub selected_subcommand: Option<usize>,
    /// Env vars to inject when spawning. Defaults to `RUST_LOG=debug` so
    /// the TUI doesn't silence anything by accident.
    pub env: Vec<EnvVar>,
}

#[derive(Debug, Clone)]
pub struct SubcommandFlags {
    pub name: String,
    pub about: Option<String>,
    pub flags: Vec<FlagState>,
}

impl ProgramFlags {
    pub fn from_spec(spec: &ProgramSpec) -> Self {
        let global = spec.arg_specs().into_iter().map(FlagState::new).collect();
        let subcommands: Vec<SubcommandFlags> = spec
            .subcommand_specs()
            .into_iter()
            .map(subcommand_flags_from_spec)
            .collect();
        let selected_subcommand = if subcommands.is_empty() { None } else { Some(0) };
        Self {
            global,
            subcommands,
            selected_subcommand,
            env: default_env(),
        }
    }

    /// Argv tail to pass to the binary, in order. Does NOT include
    /// `cargo run -p <pkg> --`; that's the caller's responsibility.
    pub fn build_argv(&self) -> Vec<String> {
        let mut argv = Vec::new();
        push_overridden(&self.global, &mut argv);
        if let Some(idx) = self.selected_subcommand {
            if let Some(sub) = self.subcommands.get(idx) {
                argv.push(sub.name.clone());
                push_overridden(&sub.flags, &mut argv);
            }
        }
        argv
    }

    /// Env vars to apply at spawn time. Entries with an empty key are
    /// skipped (they're placeholders from a partly-typed Add).
    pub fn build_env(&self) -> Vec<(String, String)> {
        self.env
            .iter()
            .filter(|e| !e.key.is_empty())
            .map(|e| (e.key.clone(), e.value.clone()))
            .collect()
    }

    /// Currently-selected flag list view: global + any selected-subcommand
    /// flags, with a marker that lets the UI render them in groups.
    pub fn visible_flags_mut(&mut self) -> Vec<FlagRow<'_>> {
        let mut out = Vec::new();
        for f in &mut self.global {
            out.push(FlagRow::Flag(f));
        }
        if let Some(idx) = self.selected_subcommand {
            if let Some(sub) = self.subcommands.get_mut(idx) {
                // Inserted as a synthetic header row so the UI can show the
                // subcommand context inline.
                // (We can't borrow `sub` immutably and `sub.flags` mutably
                // simultaneously, so the header is produced from owned data.)
                let header = format!("── subcommand: {} ──", sub.name);
                out.push(FlagRow::Header(header));
                for f in &mut sub.flags {
                    out.push(FlagRow::Flag(f));
                }
            }
        }
        out
    }
}

pub enum FlagRow<'a> {
    #[allow(dead_code)]
    Header(String),
    Flag(&'a mut FlagState),
}

fn default_env() -> Vec<EnvVar> {
    vec![EnvVar {
        key: "RUST_LOG".to_string(),
        value: "debug".to_string(),
    }]
}

fn subcommand_flags_from_spec(spec: SubcommandSpec) -> SubcommandFlags {
    SubcommandFlags {
        name: spec.name,
        about: spec.about,
        flags: spec.args.into_iter().map(FlagState::new).collect(),
    }
}

fn push_overridden(flags: &[FlagState], out: &mut Vec<String>) {
    for f in flags {
        if !f.overridden {
            continue;
        }
        let name = f.spec.display_name();
        match &f.value {
            FlagValue::Bool(true) => out.push(name),
            FlagValue::Bool(false) => {} // off == omit
            FlagValue::Text(s) if s.is_empty() => {}
            FlagValue::Text(s) => {
                out.push(name);
                out.push(s.clone());
            }
        }
    }
}
