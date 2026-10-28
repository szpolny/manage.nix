//! Deployments: creating them, building them, and finishing them.
//!
//! A deploy is queued, then claimed by one server's worker, which fetches
//! the repository, builds every machine's system, signs it, pushes it to
//! the cache, and releases it to the machines. Agent sessions (on any
//! server) hand released activations to their agents and record what the
//! agents report. A sweeper fails activations and builds that take too
//! long. Rollbacks skip the build: they are released at once.

use std::collections::HashSet;
use std::time::Duration;

use axum::http::StatusCode;
use mnix_protocol::api::{
    CreateDeployment, CreateRollback, DeploymentKind, DeploymentStatus, MachineDeploymentStatus,
};
use mnix_protocol::{ActivationReport, ActivationStatus};
use mnix_store::{Machine, MachineOutcome, NewDeployment, NewDeploymentMachine, Release};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::app::{ApiError, AppState, State};
use crate::build::{BuildOn, Log};

/// How often the worker looks for queued deployments without being told.
const QUEUE_POLL: Duration = Duration::from_secs(5);
/// How often the sweeper looks for stuck deployments.
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);
/// The longest `requested_by` and `rev` accepted.
const MAX_TEXT: usize = 255;

/// Selects machines for `targets`: names or IDs, or `tag:<tag>`.
fn select_machines(all: &[Machine], targets: &[String]) -> Result<Vec<Machine>, ApiError> {
    if targets.is_empty() {
        return Err(ApiError::bad_request("no machines to deploy to"));
    }
    let mut seen = HashSet::new();
    let mut selected = Vec::new();
    for target in targets {
        let matching: Vec<&Machine> = match target.strip_prefix("tag:") {
            Some(tag) => all
                .iter()
                .filter(|m| m.tags.iter().any(|t| t == tag))
                .collect(),
            None => all
                .iter()
                .filter(|m| m.name == *target || m.id.to_string() == *target)
                .collect(),
        };
        if matching.is_empty() {
            return Err(ApiError::bad_request(match target.strip_prefix("tag:") {
                Some(tag) => format!("no machine has the tag {tag:?}"),
                None => format!("unknown machine {target:?}"),
            }));
        }
        for machine in matching {
            if seen.insert(machine.id) {
                selected.push(machine.clone());
            }
        }
    }
    selected.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(selected)
}

fn requested_by(value: Option<String>) -> Result<String, ApiError> {
    let value = value.unwrap_or_else(|| "admin".to_owned());
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_TEXT || value.contains(char::is_control) {
        return Err(ApiError::bad_request("invalid requested_by"));
    }
    Ok(value.to_owned())
}

/// Branches, tags, commits, and expressions like `main~1`; nothing that
/// could be taken for an option.
fn validate_rev(rev: &str) -> Result<(), ApiError> {
    let valid = !rev.is_empty()
        && rev.len() <= MAX_TEXT
        && !rev.starts_with('-')
        && !rev.contains(|c: char| c.is_whitespace() || c.is_control());
    if valid {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!("invalid revision {rev:?}")))
    }
}

/// `POST /api/deployments`
pub async fn create(state: &AppState, request: CreateDeployment) -> Result<Uuid, ApiError> {
    let rev = request.rev.unwrap_or_else(|| "HEAD".to_owned());
    validate_rev(&rev)?;
    let requested_by = requested_by(request.requested_by)?;
    let all = state.store.machines(state.organization.id).await?;
    let machines = select_machines(&all, &request.targets)?;
    let mut repository_id = None;
    for machine in &machines {
        if machine.enrolled_at.is_none() {
            return Err(ApiError::bad_request(format!(
                "{} has not enrolled yet",
                machine.name
            )));
        }
        let (Some(repo), Some(_)) = (machine.repository_id, &machine.configuration) else {
            return Err(ApiError::bad_request(format!(
                "{} has no configuration; set one with `mnix machine configure`",
                machine.name
            )));
        };
        match repository_id {
            None => repository_id = Some(repo),
            Some(other) if other != repo => {
                return Err(ApiError::bad_request(
                    "the machines are built from different repositories; deploy them separately",
                ))
            }
            Some(_) => {}
        }
    }
    let id = Uuid::new_v4();
    state
        .store
        .create_deployment(&NewDeployment {
            id,
            organization_id: state.organization.id,
            kind: DeploymentKind::Deploy,
            status: DeploymentStatus::Queued,
            repository_id,
            rev: Some(rev.clone()),
            mode: request.mode,
            requested_by: requested_by.clone(),
            created_at: OffsetDateTime::now_utc(),
            machines: machines
                .iter()
                .map(|m| NewDeploymentMachine {
                    machine_id: m.id,
                    configuration: m.configuration.clone(),
                    status: MachineDeploymentStatus::Queued,
                })
                .collect(),
        })
        .await?;
    for machine in &machines {
        state
            .event(
                Some(machine.id),
                "deployment_created",
                serde_json::json!({ "deployment_id": id, "rev": rev, "requested_by": requested_by }),
            )
            .await;
    }
    tracing::info!(
        "deployment {id}: {rev} to {} machine(s), requested by {requested_by}",
        machines.len()
    );
    state.queue.notify_one();
    Ok(id)
}

/// `POST /api/machines/:machine/rollback`
pub async fn create_rollback(
    state: &AppState,
    machine: &Machine,
    request: CreateRollback,
) -> Result<Uuid, ApiError> {
    let requested_by = requested_by(request.requested_by)?;
    if machine.enrolled_at.is_none() {
        return Err(ApiError::bad_request(format!(
            "{} has not enrolled yet",
            machine.name
        )));
    }
    let id = Uuid::new_v4();
    state
        .store
        .create_deployment(&NewDeployment {
            id,
            organization_id: state.organization.id,
            kind: DeploymentKind::Rollback,
            status: DeploymentStatus::Activating,
            repository_id: None,
            rev: None,
            mode: request.mode,
            requested_by: requested_by.clone(),
            created_at: OffsetDateTime::now_utc(),
            machines: vec![NewDeploymentMachine {
                machine_id: machine.id,
                configuration: None,
                status: MachineDeploymentStatus::Activating,
            }],
        })
        .await?;
    state
        .event(
            Some(machine.id),
            "rollback_created",
            serde_json::json!({ "deployment_id": id, "requested_by": requested_by }),
        )
        .await;
    tracing::info!("deployment {id}: roll back {}", machine.name);
    state.activations_changed();
    Ok(id)
}

/// Builds queued deployments, one at a time, until the server stops.
pub async fn run_worker(state: State) {
    let mut shutdown = state.shutdown.clone();
    loop {
        match state
            .store
            .claim_deployment(OffsetDateTime::now_utc())
            .await
        {
            Ok(Some(id)) => {
                build(&state, id).await;
                continue;
            }
            Ok(None) => {}
            Err(e) => tracing::error!("claiming a deployment: {e}"),
        }
        tokio::select! {
            _ = state.queue.notified() => {}
            _ = tokio::time::sleep(QUEUE_POLL) => {}
            _ = shutdown.wait_for(|stopping| *stopping) => return,
        }
    }
}

async fn build(state: &AppState, id: Uuid) {
    let mut log = Log::new(state.store.clone(), id, None);
    let timeout = state.settings.build_timeout;
    let result = match tokio::time::timeout(timeout, build_machines(state, id, &mut log)).await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "the build did not finish within {}",
            humantime::format_duration(timeout)
        )),
    };
    if let Err(e) = result {
        let message = format!("{e:#}");
        tracing::error!("deployment {id}: {message}");
        log.line(format!("error: {message}")).await;
        if let Err(e) = state
            .store
            .fail_deployment(id, &message, OffsetDateTime::now_utc())
            .await
        {
            tracing::error!("deployment {id}: {e}");
        }
    }
    log.flush().await;
}

/// Fetches the revision, builds each machine's system, and releases the
/// ones that built. A machine whose build fails fails on its own.
async fn build_machines(state: &AppState, id: Uuid, log: &mut Log) -> anyhow::Result<()> {
    let store = &state.store;
    let deployment = store
        .deployment(id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the deployment disappeared"))?;
    let repository = match deployment.repository_id {
        Some(repo) => store.repository(repo).await?,
        None => None,
    }
    .ok_or_else(|| anyhow::anyhow!("the deployment has no repository"))?;
    let builder = &state.builder;

    let mirror = builder.fetch(repository.id, &repository.url, log).await?;
    let rev = deployment.rev.as_deref().unwrap_or("HEAD");
    let commit = builder.resolve(&mirror, rev, log).await?;
    log.line(format!("{rev} is commit {commit}")).await;
    store.set_deployment_commit(id, &commit).await?;
    log.flush().await;

    if builder.build_on == BuildOn::Machine {
        return release_builds(state, &deployment, log).await;
    }

    let mut built = Vec::new();
    for machine in &deployment.machines {
        if machine.status != MachineDeploymentStatus::Queued {
            continue;
        }
        let Some(configuration) = &machine.configuration else {
            continue;
        };
        store
            .set_deployment_machine_status(
                id,
                machine.machine_id,
                MachineDeploymentStatus::Building,
            )
            .await?;
        let mut machine_log = Log::new(store.clone(), id, Some(machine.machine_id));
        let result = builder
            .build(
                &mirror,
                &commit,
                machine.machine_id,
                configuration,
                &mut machine_log,
            )
            .await;
        match result {
            Ok(system) => built.push((machine.machine_id, system)),
            Err(e) => {
                let message = format!("build failed: {e:#}");
                tracing::warn!("deployment {id}: {}: {message}", machine.machine_name);
                machine_log.line(format!("error: {message}")).await;
                finish(
                    state,
                    id,
                    machine.machine_id,
                    MachineOutcome {
                        status: MachineDeploymentStatus::Failed,
                        message: Some(message),
                        report: None,
                    },
                )
                .await;
            }
        }
        machine_log.flush().await;
    }

    let now = OffsetDateTime::now_utc();
    for (machine_id, system) in &built {
        store
            .release_activation(
                id,
                *machine_id,
                &Release::System {
                    store_path: system.store_path.to_string(),
                    signatures: vec![system.signature.clone()],
                },
                now,
            )
            .await?;
    }
    if !built.is_empty() {
        log.line(format!("activating on {} machine(s)", built.len()))
            .await;
        store.start_activating(id).await?;
        state.activations_changed();
    }
    Ok(())
}

/// Hands each machine the flake output to build at the deployment's commit,
/// chosen by the kind of system it reported.
async fn release_builds(
    state: &AppState,
    deployment: &mnix_store::Deployment,
    log: &mut Log,
) -> anyhow::Result<()> {
    let store = &state.store;
    let now = OffsetDateTime::now_utc();
    let mut released = 0;
    for machine in &deployment.machines {
        if machine.status != MachineDeploymentStatus::Queued {
            continue;
        }
        let Some(configuration) = &machine.configuration else {
            continue;
        };
        let os = store
            .machine(machine.machine_id)
            .await?
            .and_then(|m| m.facts)
            .map(|f| f.os)
            .unwrap_or_default();
        let attribute = os.flake_attribute(configuration);
        log.line(format!("{} builds {attribute}", machine.machine_name))
            .await;
        store
            .release_activation(
                deployment.id,
                machine.machine_id,
                &Release::Build { attribute },
                now,
            )
            .await?;
        released += 1;
    }
    if released > 0 {
        log.line(format!("activating on {released} machine(s)"))
            .await;
        store.start_activating(deployment.id).await?;
        state.activations_changed();
    }
    Ok(())
}

/// Records how a machine's part ended, and the deployment's result when it
/// was the last one.
pub async fn finish(
    state: &AppState,
    deployment_id: Uuid,
    machine_id: Uuid,
    outcome: MachineOutcome,
) {
    let status = outcome.status;
    let result = state
        .store
        .finish_deployment_machine(
            deployment_id,
            machine_id,
            &outcome,
            OffsetDateTime::now_utc(),
        )
        .await;
    match result {
        Ok(finished) if finished.machine => {
            state
                .event(
                    Some(machine_id),
                    "deployment_finished",
                    serde_json::json!({
                        "deployment_id": deployment_id,
                        "status": status.as_str(),
                        "message": outcome.message,
                    }),
                )
                .await;
            if let Some(status) = finished.deployment {
                tracing::info!("deployment {deployment_id}: {status}");
            }
        }
        Ok(_) => tracing::info!(
            "deployment {deployment_id}: ignoring a late result ({status}) for machine {machine_id}"
        ),
        Err(e) => tracing::error!("deployment {deployment_id}: {e}"),
    }
}

/// The deployment status an agent's report means for its machine.
pub fn outcome(report: ActivationReport) -> MachineOutcome {
    let status = match report.status {
        ActivationStatus::Succeeded => MachineDeploymentStatus::Succeeded,
        ActivationStatus::RolledBack => MachineDeploymentStatus::RolledBack,
        ActivationStatus::Failed => MachineDeploymentStatus::Failed,
    };
    MachineOutcome {
        status,
        message: report.message.clone(),
        report: Some(report),
    }
}

/// Fails activations that took longer than the activation timeout (the
/// machine never came online, or never answered) and builds whose server
/// died.
pub async fn run_sweeper(state: State) {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        if let Err(e) = sweep(&state).await {
            tracing::error!("checking for stuck deployments: {e}");
        }
    }
}

async fn sweep(state: &AppState) -> mnix_store::Result<()> {
    let now = OffsetDateTime::now_utc();
    let timeout = state.settings.activation_timeout;
    for (deployment_id, machine_id) in state.store.expired_activations(now - timeout).await? {
        tracing::warn!("deployment {deployment_id}: machine {machine_id} timed out");
        finish(
            state,
            deployment_id,
            machine_id,
            MachineOutcome {
                status: MachineDeploymentStatus::Failed,
                message: Some(format!(
                    "the machine did not finish activating within {}",
                    humantime::format_duration(timeout)
                )),
                report: None,
            },
        )
        .await;
    }
    // The worker gives up on its own after the build timeout; a build
    // older than twice that belongs to a server that died.
    for id in state
        .store
        .stale_builds(now - state.settings.build_timeout * 2)
        .await?
    {
        tracing::warn!("deployment {id}: its build was abandoned");
        state
            .store
            .fail_deployment(id, "the build was interrupted (the server stopped)", now)
            .await?;
    }
    Ok(())
}

/// Fails every deployment still building when a single-instance (SQLite)
/// server starts: nobody else can be building it.
pub async fn fail_interrupted_builds(state: &AppState) -> mnix_store::Result<()> {
    let now = OffsetDateTime::now_utc();
    for id in state.store.stale_builds(now).await? {
        tracing::warn!("deployment {id}: interrupted by a restart");
        state
            .store
            .fail_deployment(id, "the build was interrupted (the server restarted)", now)
            .await?;
    }
    Ok(())
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("{e:#}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(name: &str, tags: &[&str]) -> Machine {
        Machine {
            id: Uuid::new_v4(),
            organization_id: Uuid::nil(),
            name: name.to_owned(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            enrolled_at: None,
            agent_version: None,
            facts: None,
            facts_updated_at: None,
            last_seen_at: None,
            connected: false,
            repository_id: None,
            configuration: None,
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn targets_select_machines_by_name_id_and_tag() {
        let all = [
            machine("web-1", &["web"]),
            machine("web-2", &["web"]),
            machine("db", &[]),
        ];
        let names = |targets: &[&str]| {
            let targets: Vec<String> = targets.iter().map(|t| t.to_string()).collect();
            select_machines(&all, &targets)
                .map(|ms| ms.into_iter().map(|m| m.name).collect::<Vec<_>>())
        };
        assert_eq!(names(&["db"]).unwrap(), ["db"]);
        assert_eq!(names(&["tag:web", "web-1"]).unwrap(), ["web-1", "web-2"]);
        assert_eq!(names(&[&all[2].id.to_string()]).unwrap(), ["db"]);
        assert!(names(&[]).is_err());
        assert!(names(&["nope"]).is_err());
        assert!(names(&["tag:nope"]).is_err());
    }

    #[test]
    fn revisions_cannot_look_like_options() {
        for ok in ["main", "v1.2", "HEAD~1", "0123abc", "refs/heads/x"] {
            assert!(validate_rev(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-c", "--upload-pack=x", "a b", "a\nb"] {
            assert!(validate_rev(bad).is_err(), "{bad:?}");
        }
    }
}
