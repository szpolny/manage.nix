//! `mnix-agent`: runs as root on managed machines.
//!
//! The agent only accepts the commands defined in the protocol and never
//! runs arbitrary shell. Subcommands arrive with M1.

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "mnix-agent", version, about)]
struct Cli {}

fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    println!(
        "mnix-agent {} (protocol {})",
        env!("CARGO_PKG_VERSION"),
        mnix_protocol::PROTOCOL_VERSION
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }
}
