//! `mnix-server`: the manage.nix control plane.
//!
//! The HTTP API and agent endpoint arrive with M2. For now the server can
//! open its database and apply migrations.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "mnix-server", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Apply database migrations and exit.
    Migrate {
        /// `sqlite://...` or `postgres://...`
        #[arg(long, env = "MNIX_DATABASE_URL")]
        database_url: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => println!(
            "mnix-server {} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            mnix_protocol::PROTOCOL_VERSION
        ),
        Some(Command::Migrate { database_url }) => {
            let store = mnix_store::connect(&database_url).await?;
            store.migrate().await?;
            println!("migrated {:?} database", store.backend());
        }
    }
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
