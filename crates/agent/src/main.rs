//! `mnix-agent`: runs as root on managed machines.
//!
//! The agent only accepts the commands defined here and in the protocol,
//! and never runs arbitrary shell.

mod activate;
mod config;
mod credential;
mod daemon;
mod facts;
mod health;
mod host;
mod signature;
mod store_path;

use std::fs::File;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use mnix_protocol::{ActivationMode, ActivationReport, ActivationStatus};

/// Serializes activations on this machine.
const LOCK_PATH: &str = "/run/manage-nix/activate.lock";

#[derive(Debug, Parser)]
#[command(name = "mnix-agent", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Activate a NixOS system, rolling back if it is not healthy.
    ///
    /// Prints a JSON report on stdout. Exits 0 on success, 1 if the request
    /// was refused or failed before the system changed, 2 if the system was
    /// rolled back, and 3 if rolling back failed.
    Activate {
        /// The system's top-level store path.
        store_path: String,
        #[arg(long, value_enum, default_value_t = Mode::Switch)]
        mode: Mode,
    },
    /// Print facts about this machine as JSON.
    Facts,
    /// Connect to the control plane and stay connected.
    ///
    /// On first start, exchanges the enrollment token for a credential and
    /// stores it in the state directory.
    Run {
        /// The control plane's URL, for example `https://mnix.example.com`.
        #[arg(long, env = "MNIX_SERVER_URL")]
        server_url: String,
        /// File containing a one-time enrollment token from
        /// `mnix machine add`. Only read while the machine is not enrolled.
        #[arg(long, env = "MNIX_ENROLLMENT_TOKEN_FILE")]
        enrollment_token_file: Option<PathBuf>,
        /// Where the agent keeps its credential.
        #[arg(long, default_value = "/var/lib/manage-nix")]
        state_dir: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Mode {
    /// Make it the boot default and activate it now.
    Switch,
    /// Make it the boot default only.
    Boot,
    /// Activate it now without changing the boot default.
    Test,
}

impl From<Mode> for ActivationMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Switch => Self::Switch,
            Mode::Boot => Self::Boot,
            Mode::Test => Self::Test,
        }
    }
}

fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match cli.command {
        None => {
            println!(
                "mnix-agent {} (protocol {})",
                env!("CARGO_PKG_VERSION"),
                mnix_protocol::PROTOCOL_VERSION
            );
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Facts) => {
            println!("{}", serde_json::to_string_pretty(&facts::collect()?)?);
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Run {
            server_url,
            enrollment_token_file,
            state_dir,
        }) => {
            tokio::runtime::Runtime::new()?.block_on(daemon::run(daemon::Options {
                server_url,
                enrollment_token_file,
                state_dir,
            }))?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Activate { store_path, mode }) => {
            let _lock = lock()?;
            let report = activate::activate(&host::NixosHost, &store_path, mode.into());
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(exit_code(&report))
        }
    }
}

fn lock() -> anyhow::Result<File> {
    let path = std::path::Path::new(LOCK_PATH);
    std::fs::create_dir_all(path.parent().unwrap_or(path))?;
    let file = File::create(path).with_context(|| format!("creating {LOCK_PATH}"))?;
    file.try_lock()
        .map_err(|_| anyhow::anyhow!("another activation is already running ({LOCK_PATH})"))?;
    Ok(file)
}

fn exit_code(report: &ActivationReport) -> ExitCode {
    match (report.status, report.system_changed) {
        (ActivationStatus::Succeeded, _) => ExitCode::SUCCESS,
        (ActivationStatus::Failed, false) => ExitCode::from(1),
        (ActivationStatus::RolledBack, _) => ExitCode::from(2),
        (ActivationStatus::Failed, true) => ExitCode::from(3),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn mode_defaults_to_switch() {
        let cli = Cli::try_parse_from(["mnix-agent", "activate", "/nix/store/x"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Activate {
                mode: Mode::Switch,
                ..
            })
        ));
    }
}
