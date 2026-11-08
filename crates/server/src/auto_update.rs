//! Updates the server applies to machines on its own
//! (`PUT /api/machines/:machine/auto-update`).
//!
//! Every [`POLL`], each machine with auto-updates that is online and has no
//! deployment in progress gets at most one of them, if it is due: one has
//! not been started for the machine within [`INTERVAL`], and there is
//! something to update. Flatpak updates run first; the flake input update
//! follows at a later poll. Deployments the scheduler starts are
//! requested by [`REQUESTED_BY`], which is how it finds its last attempt.
//! Machines can independently opt into prebuilding available Nix updates,
//! using the preview builder without activation or repository changes.

use std::time::Duration;

use mnix_protocol::api::{
    CreateDeployment, CreateFlatpakUpdate, DeploymentKind, MachineDeploymentStatus, MachineStatus,
};
use mnix_protocol::ActivationMode;
use mnix_store::Machine;
use time::OffsetDateTime;

use crate::app::State;
use crate::{api, deploy, updates};

/// How often the scheduler looks for due updates.
pub const POLL: Duration = Duration::from_secs(15 * 60);
/// The least time between two automatic updates of one kind on a machine.
pub const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// `requested_by` of the deployments the scheduler starts.
pub const REQUESTED_BY: &str = "auto-update";
/// How much of each machine's history is searched for its last attempt.
const HISTORY: u32 = 100;

/// Applies due automatic updates until the server stops.
pub async fn run(state: State) {
    let mut shutdown = state.shutdown.clone();
    loop {
        // Waiting first lets agents reconnect and report their facts after
        // the server starts.
        tokio::select! {
            _ = tokio::time::sleep(POLL) => {}
            _ = shutdown.wait_for(|stopping| *stopping) => return,
        }
        if let Err(e) = run_once(&state).await {
            tracing::error!("auto-update: {e:#}");
        }
    }
}

/// One pass over every machine.
pub async fn run_once(state: &State) -> anyhow::Result<()> {
    for machine in state.store.machines(state.organization.id).await? {
        if machine.auto_update.nixpkgs
            || machine.auto_update.flatpak
            || machine.auto_update.prebuild
        {
            if let Err(e) = update(state, machine.clone()).await {
                tracing::warn!("auto-update {}: {e:#}", machine.name);
            }
        }
    }
    Ok(())
}

async fn update(state: &State, machine: Machine) -> anyhow::Result<()> {
    let now = OffsetDateTime::now_utc();
    if api::status(&machine, now, state.settings.heartbeat_timeout) != MachineStatus::Online {
        return Ok(());
    }
    let history = state.store.machine_deployments(machine.id, HISTORY).await?;
    let busy = history.iter().any(|d| {
        matches!(
            d.status,
            MachineDeploymentStatus::Queued
                | MachineDeploymentStatus::Building
                | MachineDeploymentStatus::Activating
        )
    });
    if busy {
        return Ok(());
    }
    let due = |kind: DeploymentKind| {
        !history.iter().any(|d| {
            d.kind == kind && d.requested_by == REQUESTED_BY && now - d.created_at < INTERVAL
        })
    };

    let flatpaks = machine
        .facts
        .as_ref()
        .and_then(|f| f.flatpak_updates.as_ref())
        .map_or(0, Vec::len);
    if machine.auto_update.flatpak && flatpaks > 0 && due(DeploymentKind::FlatpakUpdate) {
        let id = deploy::create_flatpak_update(
            state,
            &machine,
            CreateFlatpakUpdate {
                requested_by: Some(REQUESTED_BY.to_owned()),
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!(e.message))?;
        tracing::info!(
            "auto-update {}: {flatpaks} Flatpak update(s) in deployment {id}",
            machine.name
        );
        return Ok(());
    }

    if machine.auto_update.nixpkgs && due(DeploymentKind::Update) {
        let name = machine.name.clone();
        let target = machine.id.to_string();
        if let Some(rev) = updates::nixpkgs_update(state, machine.clone()).await? {
            let id = deploy::create(
                state,
                CreateDeployment {
                    targets: vec![target],
                    rev: Some(rev.clone()),
                    mode: ActivationMode::Switch,
                    requested_by: Some(REQUESTED_BY.to_owned()),
                    update_nixpkgs: true,
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;
            tracing::info!("auto-update {name}: flake inputs on {rev} in deployment {id}");
            return Ok(());
        }
    }
    if machine.auto_update.prebuild {
        updates::prebuild(state, machine).await?;
    }
    Ok(())
}
