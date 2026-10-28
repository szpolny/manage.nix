//! `mnix`: command-line client for the manage.nix HTTP API.

mod client;
mod deploys;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Parser, Subcommand, ValueEnum};
use mnix_protocol::api::{
    CreateDeployment, CreateEnrollmentToken, CreateMachine, CreatePreview, CreateReboot,
    CreateRepository, CreateRollback, CreatedMachine, Deployment, DeploymentStatus,
    EnrollmentToken, Machine, Preview, PreviewStatus, Repository, SetConfiguration, SetTags,
    SigningKey, Updates,
};
use mnix_protocol::ActivationMode;
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
    /// List the Git repositories machines are built from.
    Repos {
        #[arg(long)]
        json: bool,
    },
    /// Manage repositories.
    #[command(subcommand)]
    Repo(RepoCommand),
    /// Build a revision for machines and activate it, rolling back
    /// machines where it is unhealthy or cannot reach the server.
    ///
    /// Follows the deployment's log until it finishes; exits 0 only if
    /// every machine succeeded.
    Deploy {
        /// Machine names or IDs, or `tag:<tag>`.
        #[arg(required = true)]
        targets: Vec<String>,
        /// A branch, tag, or commit (default: the repository's default
        /// branch).
        #[arg(long)]
        rev: Option<String>,
        #[command(flatten)]
        options: DeployOptions,
    },
    /// Return a machine to its previous system profile generation, with the
    /// same health checks and confirmation as a deploy.
    Rollback {
        /// The machine's name or ID.
        machine: String,
        #[command(flatten)]
        options: DeployOptions,
    },
    /// Restart a machine. It must be online; the reboot finishes when its
    /// agent reports from the new boot.
    Reboot {
        /// The machine's name or ID.
        machine: String,
        #[command(flatten)]
        options: FollowOptions,
    },
    /// Show the commits a machine's repository has that its running system
    /// was not built from.
    Updates {
        /// The machine's name or ID.
        machine: String,
        /// The branch, tag, or commit to compare with (default: the branch
        /// the running system was deployed from, or the default branch).
        #[arg(long)]
        rev: Option<String>,
        /// Also build the new system on the server and print how its
        /// packages differ from the running system's.
        #[arg(long)]
        preview: bool,
        #[arg(long)]
        json: bool,
    },
    /// List recent deployments, or show one.
    Deploys {
        #[command(subcommand)]
        command: Option<DeploysCommand>,
        /// How many to list.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Print the public key the server signs systems with. Add it to
    /// `services.manage-nix.agent.trustedPublicKeys` on every machine.
    SigningKey,
}

#[derive(Debug, Args)]
struct DeployOptions {
    #[arg(long, value_enum, default_value_t = Mode::Switch)]
    mode: Mode,
    #[command(flatten)]
    follow: FollowOptions,
}

#[derive(Debug, Args)]
struct FollowOptions {
    /// Who is asking, for the history (default: $USER).
    #[arg(long = "as", value_name = "NAME")]
    requested_by: Option<String>,
    /// Print the deployment and return instead of following it.
    #[arg(long)]
    no_wait: bool,
    /// Print the deployment as JSON.
    #[arg(long)]
    json: bool,
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

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// Register a Git repository containing a flake with
    /// `nixosConfigurations`.
    Add {
        name: String,
        /// Anything `git clone` accepts.
        url: String,
    },
}

#[derive(Debug, Subcommand)]
enum DeploysCommand {
    /// Show a deployment's machines and results.
    Show {
        /// The deployment's ID, or the start of it.
        id: String,
        /// Print its log too.
        #[arg(long)]
        logs: bool,
        #[arg(long)]
        json: bool,
    },
    /// Print a deployment's log.
    Logs {
        id: String,
        /// Keep printing until the deployment finishes.
        #[arg(long, short)]
        follow: bool,
    },
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
    /// Build the machine from `nixosConfigurations.<configuration>` in a
    /// repository.
    Configure {
        /// The machine's name or ID.
        machine: String,
        /// The repository's name.
        #[arg(long)]
        repo: String,
        /// The configuration's name (default: the machine's name).
        #[arg(long)]
        configuration: Option<String>,
    },
    /// Add tags to a machine, to deploy to `tag:<tag>`.
    Tag {
        machine: String,
        #[arg(required = true)]
        tags: Vec<String>,
    },
    /// Remove tags from a machine.
    Untag {
        machine: String,
        #[arg(required = true)]
        tags: Vec<String>,
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

fn main() -> anyhow::Result<ExitCode> {
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
        Command::Machine(MachineCommand::Configure {
            machine,
            repo,
            configuration,
        }) => {
            let machine: Machine = client.put(
                &format!("/api/machines/{}/configuration", client::escape(&machine)),
                &SetConfiguration {
                    repository: repo,
                    configuration,
                },
            )?;
            eprintln!(
                "{} is built from {}#nixosConfigurations.{}",
                machine.name,
                machine.repository.as_deref().unwrap_or("?"),
                machine.configuration.as_deref().unwrap_or("?")
            );
        }
        Command::Machine(MachineCommand::Tag { machine, tags }) => {
            update_tags(&client, &machine, |current| current.extend(tags))?;
        }
        Command::Machine(MachineCommand::Untag { machine, tags }) => {
            update_tags(&client, &machine, |current| {
                current.retain(|t| !tags.contains(t))
            })?;
        }
        Command::Repos { json } => {
            let repositories: Vec<Repository> = client.get("/api/repositories")?;
            if json {
                print_json(&repositories)?;
            } else if repositories.is_empty() {
                println!("No repositories. Add one with `mnix repo add <name> <url>`.");
            } else {
                let rows: Vec<Vec<String>> = repositories
                    .iter()
                    .map(|r| vec![r.name.clone(), r.url.clone()])
                    .collect();
                print!("{}", table(&["NAME", "URL"], &rows));
            }
        }
        Command::Repo(RepoCommand::Add { name, url }) => {
            let repository: Repository =
                client.post("/api/repositories", &CreateRepository { name, url })?;
            eprintln!("Added repository {} ({})", repository.name, repository.url);
        }
        Command::Deploy {
            targets,
            rev,
            options,
        } => {
            let deployment: Deployment = client.post(
                "/api/deployments",
                &CreateDeployment {
                    targets,
                    rev,
                    mode: options.mode.into(),
                    requested_by: requested_by(&options.follow),
                },
            )?;
            return finish(&client, deployment, &options.follow);
        }
        Command::Rollback { machine, options } => {
            let deployment: Deployment = client.post(
                &format!("/api/machines/{}/rollback", client::escape(&machine)),
                &CreateRollback {
                    mode: options.mode.into(),
                    requested_by: requested_by(&options.follow),
                },
            )?;
            return finish(&client, deployment, &options.follow);
        }
        Command::Reboot { machine, options } => {
            let deployment: Deployment = client.post(
                &format!("/api/machines/{}/reboot", client::escape(&machine)),
                &CreateReboot {
                    requested_by: requested_by(&options),
                },
            )?;
            return finish(&client, deployment, &options);
        }
        Command::Updates {
            machine,
            rev,
            preview,
            json,
        } => {
            let path = format!("/api/machines/{}/updates", client::escape(&machine));
            let query = rev
                .as_deref()
                .map(|r| format!("?rev={}", client::escape(r)))
                .unwrap_or_default();
            let updates: Updates = client.get(&format!("{path}{query}"))?;
            let preview = if preview {
                Some(build_preview(&client, &path, rev)?)
            } else {
                None
            };
            if json {
                print_json(&serde_json::json!({ "updates": updates, "preview": preview }))?;
            } else {
                print!("{}", updates_text(&updates));
                if let Some(preview) = &preview {
                    print!("{}", preview_text(preview));
                }
            }
            if preview.is_some_and(|p| p.status != PreviewStatus::Succeeded) {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Deploys {
            command: None,
            limit,
            json,
        } => {
            let deployments: Vec<Deployment> =
                client.get(&format!("/api/deployments?limit={limit}"))?;
            if json {
                print_json(&deployments)?;
            } else {
                print!("{}", deploys::list(&deployments, OffsetDateTime::now_utc()));
            }
        }
        Command::Deploys {
            command: Some(DeploysCommand::Show { id, logs, json }),
            ..
        } => {
            let deployment = deploys::find(&client, &id)?;
            if json {
                print_json(&deployment)?;
            } else {
                if logs {
                    deploys::print_logs(&client, &deployment.id, 0)?;
                }
                print!("{}", deploys::show(&deployment));
            }
        }
        Command::Deploys {
            command: Some(DeploysCommand::Logs { id, follow }),
            ..
        } => {
            let deployment = deploys::find(&client, &id)?;
            if follow {
                deploys::follow(&client, &deployment.id)?;
            } else {
                deploys::print_logs(&client, &deployment.id, 0)?;
            }
        }
        Command::SigningKey => {
            let key: SigningKey = client.get("/api/signing-key")?;
            println!("{}", key.public_key);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn requested_by(options: &FollowOptions) -> Option<String> {
    options
        .requested_by
        .clone()
        .or_else(|| std::env::var("USER").ok())
        .filter(|name| !name.is_empty())
}

/// Follows a new deployment unless told not to; the exit code says whether
/// it succeeded.
fn finish(
    client: &Client,
    deployment: Deployment,
    options: &FollowOptions,
) -> anyhow::Result<ExitCode> {
    if options.no_wait {
        if options.json {
            print_json(&deployment)?;
        } else {
            println!("{}", deployment.id);
        }
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!("Deployment {}", deployment.id);
    let deployment = deploys::follow(client, &deployment.id)?;
    if options.json {
        print_json(&deployment)?;
    }
    Ok(match deployment.status {
        DeploymentStatus::Succeeded => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    })
}

/// Starts a preview of `rev` and waits for it to finish.
fn build_preview(client: &Client, path: &str, rev: Option<String>) -> anyhow::Result<Preview> {
    let path = format!("{path}/preview");
    let mut preview: Preview = client.post(&path, &CreatePreview { rev })?;
    eprintln!("Building the new system on the server…");
    while preview.status == PreviewStatus::Building {
        std::thread::sleep(Duration::from_secs(2));
        preview = client.get(&path)?;
    }
    Ok(preview)
}

fn updates_text(updates: &Updates) -> String {
    let short = |c: &str| c[..c.len().min(12)].to_owned();
    let target = format!(
        "{} {} ({})",
        updates.repository,
        updates.rev,
        short(&updates.commit)
    );
    let mut out = match (updates.behind, &updates.current_commit) {
        (Some(0), _) => format!("Up to date with {target}.\n"),
        (Some(behind), _) => format!(
            "{behind} commit{} behind {target}:\n",
            if behind == 1 { "" } else { "s" }
        ),
        (None, Some(current)) => format!(
            "The running commit {} is not in {}; cannot compare with {target}.\n",
            short(current),
            updates.repository
        ),
        (None, None) => format!(
            "The running system's commit is unknown (not deployed by manage.nix, and \
             no system.configurationRevision); {target} is the latest.\n"
        ),
    };
    for commit in &updates.commits {
        out.push_str(&format!(
            "  {}  {}  ({}, {})\n",
            short(&commit.commit),
            commit.summary,
            commit.author,
            commit.date
        ));
    }
    if let Some(ahead) = updates.ahead.filter(|&n| n > 0) {
        out.push_str(&format!(
            "The running system has {ahead} commit{} that {} lacks.\n",
            if ahead == 1 { "" } else { "s" },
            updates.rev
        ));
    }
    out
}

fn preview_text(preview: &Preview) -> String {
    match preview.status {
        PreviewStatus::Building => "The preview is still building.\n".to_owned(),
        PreviewStatus::Failed => format!(
            "The preview failed: {}\n",
            preview.message.as_deref().unwrap_or("unknown error")
        ),
        PreviewStatus::Succeeded => match preview.diff.as_deref().map(str::trim) {
            Some("") | None => "\nThe new system has the same packages.\n".to_owned(),
            Some(diff) => format!("\nPackage changes:\n{diff}\n"),
        },
    }
}

fn update_tags(
    client: &Client,
    machine: &str,
    change: impl FnOnce(&mut Vec<String>),
) -> anyhow::Result<()> {
    let current: Machine = client.get(&format!("/api/machines/{}", client::escape(machine)))?;
    let mut tags = current.tags;
    change(&mut tags);
    tags.sort();
    tags.dedup();
    let updated: Machine = client.put(
        &format!("/api/machines/{}/tags", client::escape(machine)),
        &SetTags { tags },
    )?;
    eprintln!("{}: {}", updated.name, updated.tags.join(", "));
    Ok(())
}

/// Formats rows under a header, padding every column but the last.
pub fn table<S: AsRef<str>>(header: &[&str], rows: &[Vec<S>]) -> String {
    let widths: Vec<usize> = (0..header.len())
        .map(|i| {
            rows.iter()
                .map(|r| r[i].as_ref().len())
                .chain([header[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    let header: Vec<&str> = header.to_vec();
    for cells in
        std::iter::once(header).chain(rows.iter().map(|r| r.iter().map(AsRef::as_ref).collect()))
    {
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{cell:<w$}  ", w = widths[i]));
            }
        }
        out.push('\n');
    }
    out
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
pub fn ago(timestamp: &str, now: OffsetDateTime) -> String {
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
            repository: None,
            configuration: None,
            tags: Vec::new(),
            revision: None,
            last_deployment: None,
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
