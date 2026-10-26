//! `mnix`: command-line client for the manage.nix HTTP API.

mod client;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use mnix_protocol::api::{
    CreateEnrollmentToken, CreateMachine, CreatedMachine, EnrollmentToken, Machine,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::client::Client;

#[derive(Debug, Parser)]
#[command(name = "mnix", version, about)]
struct Cli {
    #[command(flatten)]
    connection: Connection,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Args)]
struct Connection {
    /// The control plane's URL.
    #[arg(
        long,
        global = true,
        env = "MNIX_SERVER",
        default_value = "http://127.0.0.1:8080"
    )]
    server: String,
    /// File containing the admin API token.
    #[arg(long, global = true, env = "MNIX_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// The admin API token (prefer --token-file; arguments are visible to
    /// other users).
    #[arg(long, global = true, env = "MNIX_TOKEN", hide_env_values = true)]
    token: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List machines with their status and current system.
    Machines {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Manage one machine.
    #[command(subcommand)]
    Machine(MachineCommand),
}

#[derive(Debug, Subcommand)]
enum MachineCommand {
    /// Add a machine and print its one-time enrollment token.
    ///
    /// Give the token to the machine's agent
    /// (`services.manage-nix.agent.enrollmentTokenFile`). Only the token is
    /// printed on stdout, so it can be captured by scripts.
    Add {
        name: String,
        #[command(flatten)]
        token: TokenArgs,
    },
    /// Print a new enrollment token for an existing machine, for example
    /// because the first one expired or the machine must enroll again.
    Token {
        /// The machine's name or ID.
        machine: String,
        #[command(flatten)]
        token: TokenArgs,
    },
    /// Show a machine's details and facts as JSON.
    Show {
        /// The machine's name or ID.
        machine: String,
    },
}

#[derive(Debug, Args)]
struct TokenArgs {
    /// How long the enrollment token is valid (the server's default is 1h).
    #[arg(long, value_parser = humantime::parse_duration)]
    expires_in: Option<Duration>,
    /// Print the full response as JSON.
    #[arg(long)]
    json: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let client = Client::new(&cli.connection.server, admin_token(&cli.connection)?);
    match cli.command {
        Command::Machines { json } => {
            let machines: Vec<Machine> = client.get("/api/machines")?;
            if json {
                print_json(&machines)?;
            } else {
                let color = std::io::stdout().is_terminal();
                print!(
                    "{}",
                    machine_table(&machines, OffsetDateTime::now_utc(), color)
                );
            }
        }
        Command::Machine(MachineCommand::Add { name, token }) => {
            let created: CreatedMachine = client.post(
                "/api/machines",
                &CreateMachine {
                    name,
                    enrollment_token_ttl_secs: token.expires_in.map(|d| d.as_secs().max(1)),
                },
            )?;
            if token.json {
                print_json(&created)?;
            } else {
                eprintln!(
                    "Added {} ({}). Its enrollment token expires at {}:",
                    created.machine.name, created.machine.id, created.enrollment_token.expires_at
                );
                println!("{}", created.enrollment_token.token);
            }
        }
        Command::Machine(MachineCommand::Token { machine, token }) => {
            let created: EnrollmentToken = client.post(
                &format!(
                    "/api/machines/{}/enrollment-tokens",
                    client::escape(&machine)
                ),
                &CreateEnrollmentToken {
                    ttl_secs: token.expires_in.map(|d| d.as_secs().max(1)),
                },
            )?;
            if token.json {
                print_json(&created)?;
            } else {
                eprintln!(
                    "Enrollment token for {machine}, expires at {}:",
                    created.expires_at
                );
                println!("{}", created.token);
            }
        }
        Command::Machine(MachineCommand::Show { machine }) => {
            let machine: Machine =
                client.get(&format!("/api/machines/{}", client::escape(&machine)))?;
            print_json(&machine)?;
        }
    }
    Ok(())
}

fn admin_token(connection: &Connection) -> anyhow::Result<String> {
    if let Some(token) = &connection.token {
        return Ok(token.clone());
    }
    let path = connection.token_file.as_ref().context(
        "no admin token: pass --token-file (or set MNIX_TOKEN_FILE) to the server's admin token file",
    )?;
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("reading the admin token from {}", path.display()))?;
    Ok(token.trim().to_owned())
}

fn print_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Formats machines as a table, coloring the status column if `color`.
fn machine_table(machines: &[Machine], now: OffsetDateTime, color: bool) -> String {
    if machines.is_empty() {
        return "No machines. Add one with `mnix machine add <name>`.\n".to_owned();
    }
    let rows: Vec<[String; 4]> = machines
        .iter()
        .map(|m| {
            [
                m.name.clone(),
                m.status.to_string(),
                m.last_seen_at
                    .as_deref()
                    .map_or_else(|| "never".to_owned(), |t| ago(t, now)),
                m.current_system.clone().unwrap_or_else(|| "-".to_owned()),
            ]
        })
        .collect();
    let header = ["NAME", "STATUS", "LAST SEEN", "CURRENT SYSTEM"];
    let widths: Vec<usize> = (0..header.len())
        .map(|i| {
            rows.iter()
                .map(|r| r[i].len())
                .chain([header[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    let mut line = |cells: [&str; 4], status_color: Option<&str>| {
        for (i, cell) in cells.iter().enumerate() {
            let last = i == cells.len() - 1;
            let padded = if last {
                (*cell).to_owned()
            } else {
                format!("{cell:<w$}  ", w = widths[i])
            };
            match (i, status_color) {
                (1, Some(code)) if color => out.push_str(&format!("\x1b[{code}m{padded}\x1b[0m")),
                _ => out.push_str(&padded),
            }
        }
        out.push('\n');
    };
    line(header, None);
    for (row, machine) in rows.iter().zip(machines) {
        let code = match machine.status {
            mnix_protocol::api::MachineStatus::Online => "32",
            mnix_protocol::api::MachineStatus::Offline => "31",
            mnix_protocol::api::MachineStatus::Pending => "33",
        };
        line(
            [
                row[0].as_str(),
                row[1].as_str(),
                row[2].as_str(),
                row[3].as_str(),
            ],
            Some(code),
        );
    }
    out
}

/// `2026-10-22T08:00:00Z` → `5m ago`, relative to `now`.
fn ago(timestamp: &str, now: OffsetDateTime) -> String {
    let Ok(then) = OffsetDateTime::parse(timestamp, &Rfc3339) else {
        return timestamp.to_owned();
    };
    let secs = (now - then).whole_seconds().max(0);
    match secs {
        0..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use mnix_protocol::api::MachineStatus;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn formats_relative_times() {
        let now = time::macros::datetime!(2026-10-22 08:00:00 UTC);
        assert_eq!(ago("2026-10-22T07:59:55Z", now), "5s ago");
        assert_eq!(ago("2026-10-22T07:55:00Z", now), "5m ago");
        assert_eq!(ago("2026-10-21T08:00:00Z", now), "1d ago");
        assert_eq!(ago("garbage", now), "garbage");
    }

    #[test]
    fn prints_an_aligned_table() {
        let now = time::macros::datetime!(2026-10-22 08:00:00 UTC);
        let machine = |name: &str, status, seen: Option<&str>, system: Option<&str>| Machine {
            id: "id".to_owned(),
            name: name.to_owned(),
            status,
            created_at: "2026-10-22T07:00:00Z".to_owned(),
            enrolled_at: None,
            last_seen_at: seen.map(str::to_owned),
            agent_version: None,
            current_system: system.map(str::to_owned),
            facts: None,
        };
        let table = machine_table(
            &[
                machine(
                    "web-1",
                    MachineStatus::Online,
                    Some("2026-10-22T07:59:58Z"),
                    Some("/nix/store/abc-nixos-system"),
                ),
                machine("database", MachineStatus::Pending, None, None),
            ],
            now,
            false,
        );
        assert_eq!(
            table,
            "NAME      STATUS   LAST SEEN  CURRENT SYSTEM\n\
             web-1     online   2s ago     /nix/store/abc-nixos-system\n\
             database  pending  never      -\n"
        );
        assert!(machine_table(&[], now, false).contains("mnix machine add"));
    }
}
