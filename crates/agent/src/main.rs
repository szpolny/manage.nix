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
mod job;
mod source;

use std::fs::File;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use mnix_protocol::{ActivationMode, ActivationReport, ActivationStatus, ActivationTarget};

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
    /// Activate a NixOS or nix-darwin system, rolling back if it is not
    /// healthy.
    ///
    /// Prints a JSON report on stdout. Exits 0 on success, 1 if the request
    /// was refused or failed before the system changed, 2 if the system was
    /// rolled back, and 3 if rolling back failed.
    Activate {
        /// The system's top-level store path.
        store_path: String,
        #[arg(long, value_enum, default_value_t = Mode::Switch)]
        mode: Mode,
        /// A signature on the path (`<key name>:<base64>`) to check in
        /// addition to those Nix has. Repeatable.
        #[arg(long = "signature", value_name = "SIGNATURE")]
        signatures: Vec<String>,
    },
    /// Run an activation requested by the server (started by `run`).
    #[command(hide = true)]
    ActivationJob { dir: PathBuf },
    /// Print facts about this machine as JSON.
    Facts,
    /// Connect to every control plane in the configuration
    /// (`/etc/manage-nix/agent.json`) and stay connected.
    ///
    /// On first start with each server, exchanges its enrollment token for
    /// a credential and stores it in the state directory.
    Run {
        /// Where the agent keeps its credentials and activations.
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
        // Logs often go to files (activation logs) and the journal.
        .with_ansi(std::io::stderr().is_terminal())
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
        Some(Command::Run { state_dir }) => {
            let config = config::AgentConfig::load_from(std::path::Path::new("/"))?
                .context("no /etc/manage-nix/agent.json")?;
            tokio::runtime::Runtime::new()?.block_on(daemon::run(config.servers, state_dir))?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Activate {
            store_path,
            mode,
            signatures,
        }) => {
            let _lock = lock()?;
            let request = activate::Request {
                target: ActivationTarget::System { store_path },
                mode: mode.into(),
                signatures,
                source: None,
            };
            let report = activate::activate(&host::LocalHost, &request, &activate::Unobserved);
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(exit_code(&report))
        }
        // The report goes to the job directory; stdout is the job's log.
        Some(Command::ActivationJob { dir }) => Ok(exit_code(&job::run(&dir)?)),
    }
}

fn lock_file() -> anyhow::Result<File> {
    let path = std::path::Path::new(LOCK_PATH);
    std::fs::create_dir_all(path.parent().unwrap_or(path))?;
    File::create(path).with_context(|| format!("creating {LOCK_PATH}"))
}

/// Takes the machine-wide activation lock, failing if it is held.
pub(crate) fn lock() -> anyhow::Result<File> {
    let file = lock_file()?;
    file.try_lock()
        .map_err(|_| anyhow::anyhow!("another activation is already running ({LOCK_PATH})"))?;
    Ok(file)
}

/// Takes the machine-wide activation lock, waiting while another
/// activation (perhaps from another server) holds it.
pub(crate) fn wait_for_lock() -> anyhow::Result<File> {
    let file = lock_file()?;
    if file.try_lock().is_err() {
        tracing::info!("waiting for another activation to finish");
        file.lock()
            .with_context(|| format!("locking {LOCK_PATH}"))?;
    }
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
