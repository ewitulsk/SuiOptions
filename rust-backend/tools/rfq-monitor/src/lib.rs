//! Library surface for the `rfq-monitor` binary.
//!
//! Hosts the clap [`Cli`] type and the `program_spec` entry point so the
//! control-panel TUI can introspect the binary's flags without exec'ing it.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "rfq-monitor",
    about = "Tail every retail RFQRequest hitting the quoting service."
)]
pub struct Cli {
    /// URL of the quoting service's `/rfq-stream` NDJSON endpoint.
    #[arg(short, long, default_value = "http://127.0.0.1:9002/rfq-stream")]
    pub url: String,
}

cli_spec::define_program! {
    id          = "rfq-monitor",
    cargo_pkg   = "rfq-monitor",
    working_dir = ".",
    description = "Subscribes to the quoting service's /rfq-stream NDJSON feed and prints \
                   one line per incoming retail RFQRequest. Useful for watching the live \
                   RFQ pipeline without trawling service logs.",
    cli         = crate::Cli,
}
