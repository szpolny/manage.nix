//! `mnix`: command-line client for the manage.nix HTTP API.
//!
//! Commands arrive with M2 (`mnix machine add`, `mnix machines`).

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "mnix", version, about)]
struct Cli {}

fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    println!("mnix {}", env!("CARGO_PKG_VERSION"));
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
