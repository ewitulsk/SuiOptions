//! Program-spec registry used by the control-panel TUI to introspect
//! every binary in the workspace.
//!
//! Each binary publishes a [`program_spec::ProgramSpec`] via the
//! [`define_program!`] macro, which wraps the binary's clap
//! `CommandFactory` so the TUI can read flags/subcommands lazily without
//! exec'ing the binary.

pub mod program_spec;

pub use program_spec::ProgramSpec;

/// Generate a `pub fn program_spec() -> cli_spec::ProgramSpec` for the
/// calling binary crate. The clap `CommandFactory` is wrapped so the TUI
/// can introspect flags lazily — no metadata is hand-duplicated.
///
/// ```ignore
/// cli_spec::define_program! {
///     id          = "mm-bot",
///     cargo_pkg   = "mm-bot",
///     working_dir = ".",
///     description = "Market-maker bot. Bootstraps an Account on first run.",
///     cli         = crate::Cli,
/// }
/// ```
#[macro_export]
macro_rules! define_program {
    (
        id = $id:expr,
        cargo_pkg = $pkg:expr,
        working_dir = $dir:expr,
        description = $desc:expr,
        cli = $cli:ty $(,)?
    ) => {
        pub fn program_spec() -> $crate::ProgramSpec {
            $crate::ProgramSpec::new(
                $id,
                $pkg,
                $dir,
                $desc,
                || <$cli as ::clap::CommandFactory>::command(),
            )
        }
    };
}
