//! Program metadata exposed to the control-panel TUI.
//!
//! Each binary in the workspace publishes a [`ProgramSpec`] via the
//! [`crate::define_program!`] macro. The spec wraps the binary's clap
//! [`CommandFactory`](clap::CommandFactory) so the TUI can introspect every
//! flag, default, possible-value, and subcommand without ever invoking the
//! binary — and never needs to re-declare any of that metadata.
//!
//! The clap derive on each `Cli` struct stays the single source of truth.

use clap::{Arg, ArgAction, Command};

/// Metadata for one runnable binary.
///
/// `command_factory` returns a fresh `clap::Command` on each call; the TUI
/// uses this to walk arguments and subcommands lazily. We don't store a
/// pre-built `Command` because `clap::Command` is not `Clone` in a way that
/// preserves all metadata, and rebuilding is cheap.
#[derive(Clone)]
pub struct ProgramSpec {
    /// Stable id used by the TUI for keying state (e.g. "mm-bot").
    pub id: &'static str,
    /// Cargo package name, passed to `cargo run -p <pkg>`.
    pub cargo_pkg: &'static str,
    /// Working directory the binary should be launched from, relative to the
    /// workspace root.
    pub working_dir: &'static str,
    /// One- or two-sentence summary shown in the TUI's program list.
    pub description: &'static str,
    command_factory: fn() -> Command,
}

impl ProgramSpec {
    pub const fn new(
        id: &'static str,
        cargo_pkg: &'static str,
        working_dir: &'static str,
        description: &'static str,
        command_factory: fn() -> Command,
    ) -> Self {
        Self {
            id,
            cargo_pkg,
            working_dir,
            description,
            command_factory,
        }
    }

    /// Build a fresh `clap::Command` for live introspection.
    pub fn command(&self) -> Command {
        (self.command_factory)()
    }

    /// Top-level (non-subcommand) flags, in declaration order.
    pub fn arg_specs(&self) -> Vec<ArgSpec> {
        args_of(&self.command())
    }

    /// Subcommand metadata. Empty if the binary has no subcommands.
    pub fn subcommand_specs(&self) -> Vec<SubcommandSpec> {
        subcommands_of(&self.command())
    }

    /// Long `about` / doc from the clap command, if any.
    pub fn about(&self) -> Option<String> {
        let cmd = self.command();
        cmd.get_long_about()
            .map(|s| s.to_string())
            .or_else(|| cmd.get_about().map(|s| s.to_string()))
    }
}

impl std::fmt::Debug for ProgramSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgramSpec")
            .field("id", &self.id)
            .field("cargo_pkg", &self.cargo_pkg)
            .field("working_dir", &self.working_dir)
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

/// What kind of value a flag accepts. Drives the TUI's input widget choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    /// `--foo VALUE` — single value, free text or one of `possible_values`.
    Value,
    /// `--foo` — boolean toggle, no value.
    Flag,
    /// `--foo a --foo b` — accepts multiple values.
    Multi,
}

#[derive(Debug, Clone)]
pub struct ArgSpec {
    /// clap arg id (usually the snake_case field name on the Cli struct).
    pub id: String,
    /// `--long` form, if any.
    pub long: Option<String>,
    /// `-s` form, if any.
    pub short: Option<char>,
    /// Short help text.
    pub help: Option<String>,
    /// Default value as a display string, if clap has one.
    pub default: Option<String>,
    /// Enumerated values (e.g. clap `value_enum`). Empty for free-text args.
    pub possible_values: Vec<String>,
    pub kind: ArgKind,
    pub required: bool,
}

impl ArgSpec {
    /// CLI form used for display — prefers `--long`, falls back to `-short`.
    pub fn display_name(&self) -> String {
        if let Some(l) = &self.long {
            format!("--{l}")
        } else if let Some(c) = self.short {
            format!("-{c}")
        } else {
            self.id.clone()
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubcommandSpec {
    pub name: String,
    pub about: Option<String>,
    pub args: Vec<ArgSpec>,
}

/// Pull every named (non-positional, non-builtin) argument off a `clap::Command`.
pub fn args_of(cmd: &Command) -> Vec<ArgSpec> {
    cmd.get_arguments()
        .filter(|a| {
            !a.is_positional() && a.get_id().as_str() != "help" && a.get_id().as_str() != "version"
        })
        .map(spec_of_arg)
        .collect()
}

pub fn subcommands_of(cmd: &Command) -> Vec<SubcommandSpec> {
    cmd.get_subcommands()
        .map(|sub| SubcommandSpec {
            name: sub.get_name().to_string(),
            about: sub
                .get_long_about()
                .map(|s| s.to_string())
                .or_else(|| sub.get_about().map(|s| s.to_string())),
            args: args_of(sub),
        })
        .collect()
}

fn spec_of_arg(arg: &Arg) -> ArgSpec {
    let kind = match arg.get_action() {
        ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count => ArgKind::Flag,
        ArgAction::Append => ArgKind::Multi,
        _ => ArgKind::Value,
    };

    let default = arg
        .get_default_values()
        .iter()
        .next()
        .map(|v| v.to_string_lossy().into_owned());

    let possible_values = arg
        .get_possible_values()
        .iter()
        .map(|pv| pv.get_name().to_string())
        .collect();

    let help = arg
        .get_help()
        .map(|s| s.to_string())
        .or_else(|| arg.get_long_help().map(|s| s.to_string()));

    ArgSpec {
        id: arg.get_id().to_string(),
        long: arg.get_long().map(|s| s.to_string()),
        short: arg.get_short(),
        help,
        default,
        possible_values,
        kind,
        required: arg.is_required_set(),
    }
}
