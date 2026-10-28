//! Deployments: following them as they run, and showing them.

use std::io::Write;
use std::time::Duration;

use mnix_protocol::api::{Deployment, DeploymentKind, LogEntry, MachineDeploymentStatus};
use time::OffsetDateTime;

use crate::client::{self, Client};
use crate::{ago, table};

/// How often a followed deployment is polled.
const POLL: Duration = Duration::from_secs(1);

/// Finds a deployment by its ID or a unique prefix of it.
pub fn find(client: &Client, key: &str) -> anyhow::Result<Deployment> {
    if looks_like_uuid(key) {
        return client.get(&format!("/api/deployments/{}", client::escape(key)));
    }
    let recent: Vec<Deployment> = client.get("/api/deployments?limit=200")?;
    let mut matching = recent.into_iter().filter(|d| d.id.starts_with(key));
    match (matching.next(), matching.next()) {
        (Some(d), None) => Ok(d),
        (None, _) => anyhow::bail!("no recent deployment starts with {key:?}"),
        (Some(_), Some(_)) => anyhow::bail!("{key:?} matches several deployments"),
    }
}

/// Prints the deployment's log as it grows until the deployment finishes,
/// then a summary. Returns the finished deployment.
pub fn follow(client: &Client, id: &str) -> anyhow::Result<Deployment> {
    let mut after = 0;
    loop {
        // Read the status first: once it is finished, the logs fetched
        // next are complete.
        let deployment: Deployment = client.get(&format!("/api/deployments/{id}"))?;
        after = print_logs(client, id, after)?;
        if deployment.status.is_finished() {
            print!("{}", summary(&deployment));
            return Ok(deployment);
        }
        std::thread::sleep(POLL);
    }
}

/// Prints log entries after `after`; returns the last ID printed.
pub fn print_logs(client: &Client, id: &str, mut after: i64) -> anyhow::Result<i64> {
    let mut out = std::io::stdout().lock();
    loop {
        let entries: Vec<LogEntry> =
            client.get(&format!("/api/deployments/{id}/logs?after={after}"))?;
        let Some(last) = entries.last() else {
            return Ok(after);
        };
        after = last.id;
        for entry in &entries {
            let prefix = entry.machine.as_deref().unwrap_or("deploy");
            for line in entry.text.lines() {
                writeln!(out, "[{prefix}] {line}")?;
            }
        }
        out.flush()?;
    }
}

/// What happened to each machine.
pub fn summary(deployment: &Deployment) -> String {
    let mut out = format!(
        "\nDeployment {} {}{}\n",
        deployment.id,
        deployment.status,
        deployment
            .message
            .as_deref()
            .map(|m| format!(": {m}"))
            .unwrap_or_default()
    );
    let rows: Vec<Vec<String>> = deployment
        .machines
        .iter()
        .map(|m| {
            let detail = match (m.status, &m.message, &m.store_path) {
                (MachineDeploymentStatus::Succeeded, _, Some(path)) => path.clone(),
                (_, Some(message), _) => message.clone(),
                (_, None, Some(path)) => path.clone(),
                _ => String::new(),
            };
            vec![m.machine.clone(), m.status.to_string(), detail]
        })
        .collect();
    out.push_str(&table(&["MACHINE", "STATUS", "DETAIL"], &rows));
    out
}

/// The deployments as a table, newest first.
pub fn list(deployments: &[Deployment], now: OffsetDateTime) -> String {
    if deployments.is_empty() {
        return "No deployments yet. Start one with `mnix deploy <machine> --rev <ref>`.\n"
            .to_owned();
    }
    let rows: Vec<Vec<String>> = deployments
        .iter()
        .map(|d| {
            let what = match d.kind {
                DeploymentKind::Rollback => "rollback".to_owned(),
                DeploymentKind::Reboot => "reboot".to_owned(),
                DeploymentKind::Deploy => match (&d.rev, &d.commit) {
                    (Some(rev), Some(commit)) => {
                        format!("{rev} ({})", &commit[..commit.len().min(12)])
                    }
                    (Some(rev), None) => rev.clone(),
                    _ => "-".to_owned(),
                },
            };
            let machines: Vec<&str> = d.machines.iter().map(|m| m.machine.as_str()).collect();
            vec![
                d.id[..8].to_owned(),
                d.status.to_string(),
                what,
                machines.join(","),
                d.requested_by.clone(),
                ago(&d.created_at, now),
            ]
        })
        .collect();
    table(
        &["ID", "STATUS", "REVISION", "MACHINES", "BY", "CREATED"],
        &rows,
    )
}

/// Details of one deployment, for `mnix deploys show`.
pub fn show(deployment: &Deployment) -> String {
    let mut out = String::new();
    let mut field = |name: &str, value: &str| out.push_str(&format!("{name:<13}{value}\n"));
    field("Deployment", &deployment.id);
    field("Kind", deployment.kind.as_str());
    field("Status", deployment.status.as_str());
    if let Some(repository) = &deployment.repository {
        field("Repository", repository);
    }
    if let Some(rev) = &deployment.rev {
        field("Revision", rev);
    }
    if let Some(commit) = &deployment.commit {
        field("Commit", commit);
    }
    field("Mode", deployment.mode.as_str());
    field("Requested by", &deployment.requested_by);
    field("Created", &deployment.created_at);
    if let Some(finished) = &deployment.finished_at {
        field("Finished", finished);
    }
    out.push_str(&summary(deployment));
    out
}

/// A full hyphenated UUID.
fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mnix_protocol::api::{DeploymentMachine, DeploymentStatus};
    use mnix_protocol::ActivationMode;

    fn deployment() -> Deployment {
        let machine =
            |name: &str, status, message: Option<&str>, path: Option<&str>| DeploymentMachine {
                machine_id: "m".to_owned(),
                machine: name.to_owned(),
                configuration: Some(name.to_owned()),
                status,
                phase: None,
                store_path: path.map(str::to_owned),
                message: message.map(str::to_owned),
                report: None,
                finished_at: None,
            };
        Deployment {
            id: "0b5c3a0e-6f1e-4d2a-9c1b-2f3e4d5c6b7a".to_owned(),
            kind: DeploymentKind::Deploy,
            status: DeploymentStatus::Failed,
            message: None,
            repository: Some("infra".to_owned()),
            rev: Some("main".to_owned()),
            commit: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            mode: ActivationMode::Switch,
            requested_by: "alice".to_owned(),
            created_at: "2026-10-26T08:00:00Z".to_owned(),
            started_at: None,
            finished_at: None,
            machines: vec![
                machine(
                    "db",
                    MachineDeploymentStatus::RolledBack,
                    Some("health checks did not pass within 60s"),
                    Some("/nix/store/b"),
                ),
                machine(
                    "web",
                    MachineDeploymentStatus::Succeeded,
                    None,
                    Some("/nix/store/a"),
                ),
            ],
        }
    }

    #[test]
    fn summarises_each_machine() {
        assert_eq!(
            summary(&deployment()),
            "\nDeployment 0b5c3a0e-6f1e-4d2a-9c1b-2f3e4d5c6b7a failed\n\
             MACHINE  STATUS       DETAIL\n\
             db       rolled_back  health checks did not pass within 60s\n\
             web      succeeded    /nix/store/a\n"
        );
    }

    #[test]
    fn lists_deployments() {
        let now = time::macros::datetime!(2026-10-26 08:05:00 UTC);
        assert_eq!(
            list(&[deployment()], now),
            "ID        STATUS  REVISION             MACHINES  BY     CREATED\n\
             0b5c3a0e  failed  main (0123456789ab)  db,web    alice  5m ago\n"
        );
        assert!(list(&[], now).contains("mnix deploy"));
    }

    #[test]
    fn recognises_full_ids() {
        assert!(looks_like_uuid("0b5c3a0e-6f1e-4d2a-9c1b-2f3e4d5c6b7a"));
        assert!(!looks_like_uuid("0b5c3a0e"));
    }
}
