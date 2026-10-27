//! The admin HTTP API used by `mnix` (and later the web UI). Every route
//! requires `Authorization: Bearer <admin token>`.

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use mnix_protocol::api::{
    CreateDeployment, CreateEnrollmentToken, CreateMachine, CreateRepository, CreateRollback,
    CreatedMachine, Deployment, DeploymentMachine, EnrollmentToken, LogEntry, Machine,
    MachineStatus, Repository, SetConfiguration, SetTags, SigningKey,
};
use mnix_store::NewEnrollmentToken;
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::app::{rfc3339, ApiError, State as AppState};
use crate::{deploy, secret};

/// The longest an enrollment token may be valid.
const MAX_TOKEN_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// The most deployments and log entries returned at once.
const MAX_DEPLOYMENTS: u32 = 200;
const MAX_LOG_ENTRIES: u32 = 1000;

pub fn routes(state: AppState) -> axum::Router<AppState> {
    Router::new()
        .route("/api/machines", get(list_machines).post(create_machine))
        .route("/api/machines/{machine}", get(get_machine))
        .route(
            "/api/machines/{machine}/enrollment-tokens",
            post(create_enrollment_token),
        )
        .route(
            "/api/machines/{machine}/configuration",
            put(set_configuration),
        )
        .route("/api/machines/{machine}/tags", put(set_tags))
        .route("/api/machines/{machine}/rollback", post(create_rollback))
        .route(
            "/api/repositories",
            get(list_repositories).post(create_repository),
        )
        .route(
            "/api/deployments",
            get(list_deployments).post(create_deployment),
        )
        .route("/api/deployments/{deployment}", get(get_deployment))
        .route("/api/deployments/{deployment}/logs", get(deployment_logs))
        .route("/api/signing-key", get(signing_key))
        .route_layer(middleware::from_fn_with_state(state, require_admin))
}

async fn require_admin(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(secret::bearer);
    // Comparing hashes keeps the comparison's timing independent of the
    // token's contents.
    match token {
        Some(token) if secret::hash(token) == state.admin_token_hash => Ok(next.run(request).await),
        _ => Err(ApiError::unauthorized("missing or invalid admin token")),
    }
}

/// Repository names by ID, for views.
async fn repository_names(state: &AppState) -> Result<HashMap<Uuid, String>, ApiError> {
    Ok(state
        .store
        .repositories(state.organization.id)
        .await?
        .into_iter()
        .map(|r| (r.id, r.name))
        .collect())
}

async fn list_machines(State(state): State<AppState>) -> Result<Json<Vec<Machine>>, ApiError> {
    let now = OffsetDateTime::now_utc();
    let machines = state.store.machines(state.organization.id).await?;
    let repositories = repository_names(&state).await?;
    Ok(Json(
        machines
            .iter()
            .map(|m| machine_view(m, &repositories, now, state.settings.heartbeat_timeout))
            .collect(),
    ))
}

async fn get_machine(
    State(state): State<AppState>,
    Path(machine): Path<String>,
) -> Result<Json<Machine>, ApiError> {
    let machine = find_machine(&state, &machine).await?;
    Ok(Json(machine_view(
        &machine,
        &repository_names(&state).await?,
        OffsetDateTime::now_utc(),
        state.settings.heartbeat_timeout,
    )))
}

async fn set_configuration(
    State(state): State<AppState>,
    Path(machine): Path<String>,
    Json(request): Json<SetConfiguration>,
) -> Result<Json<Machine>, ApiError> {
    let machine = find_machine(&state, &machine).await?;
    let repository = state
        .store
        .repositories(state.organization.id)
        .await?
        .into_iter()
        .find(|r| r.name == request.repository || r.id.to_string() == request.repository)
        .ok_or_else(|| {
            ApiError::bad_request(format!("unknown repository {:?}", request.repository))
        })?;
    let configuration = request
        .configuration
        .unwrap_or_else(|| machine.name.clone());
    if !is_valid_name(&configuration) {
        return Err(ApiError::bad_request(format!(
            "invalid configuration name {configuration:?}"
        )));
    }
    state
        .store
        .set_machine_configuration(machine.id, repository.id, &configuration)
        .await?;
    state
        .event(
            Some(machine.id),
            "configuration_set",
            serde_json::json!({ "repository": repository.name, "configuration": configuration }),
        )
        .await;
    get_machine(State(state), Path(machine.id.to_string())).await
}

async fn set_tags(
    State(state): State<AppState>,
    Path(machine): Path<String>,
    Json(request): Json<SetTags>,
) -> Result<Json<Machine>, ApiError> {
    let machine = find_machine(&state, &machine).await?;
    if let Some(bad) = request.tags.iter().find(|t| !is_valid_name(t)) {
        return Err(ApiError::bad_request(format!(
            "invalid tag {bad:?}: use up to 63 letters, digits, '-', '_', or '.'"
        )));
    }
    state
        .store
        .set_machine_tags(machine.id, &request.tags)
        .await?;
    get_machine(State(state), Path(machine.id.to_string())).await
}

async fn create_rollback(
    State(state): State<AppState>,
    Path(machine): Path<String>,
    Json(request): Json<CreateRollback>,
) -> Result<(StatusCode, Json<Deployment>), ApiError> {
    let machine = find_machine(&state, &machine).await?;
    let id = deploy::create_rollback(&state, &machine, request).await?;
    Ok((
        StatusCode::CREATED,
        Json(deployment_view(&state, id).await?),
    ))
}

async fn list_repositories(
    State(state): State<AppState>,
) -> Result<Json<Vec<Repository>>, ApiError> {
    let repositories = state.store.repositories(state.organization.id).await?;
    Ok(Json(repositories.iter().map(repository_view).collect()))
}

async fn create_repository(
    State(state): State<AppState>,
    Json(request): Json<CreateRepository>,
) -> Result<(StatusCode, Json<Repository>), ApiError> {
    if !is_valid_name(&request.name) {
        return Err(ApiError::bad_request(format!(
            "invalid repository name {:?}: use up to 63 letters, digits, '-', '_', or '.'",
            request.name
        )));
    }
    validate_git_url(&request.url)?;
    let repository = state
        .store
        .create_repository(
            state.organization.id,
            &request.name,
            &request.url,
            OffsetDateTime::now_utc(),
        )
        .await?;
    tracing::info!("added repository {} ({})", repository.name, repository.url);
    Ok((StatusCode::CREATED, Json(repository_view(&repository))))
}

/// URLs `git clone` accepts, except transport helpers (`ext::`), which can
/// run commands, and anything that could be read as an option.
fn validate_git_url(url: &str) -> Result<(), ApiError> {
    let known = ["https://", "http://", "ssh://", "git://", "file:///", "/"]
        .iter()
        .any(|prefix| url.starts_with(prefix))
        // scp-like `user@host:path`
        || url.split_once(':').is_some_and(|(host, _)| !host.is_empty() && !host.contains('/'));
    let valid = known
        && url.len() <= 2048
        && !url.starts_with('-')
        && !url.contains("::")
        && !url.contains(|c: char| c.is_whitespace() || c.is_control());
    if valid {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "invalid Git URL {url:?}: use https://, ssh://, git://, file:///, user@host:path, \
             or an absolute path"
        )))
    }
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    limit: Option<u32>,
}

async fn list_deployments(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<Deployment>>, ApiError> {
    let limit = query.limit.unwrap_or(20).clamp(1, MAX_DEPLOYMENTS);
    let deployments = state
        .store
        .deployments(state.organization.id, limit)
        .await?;
    let repositories = repository_names(&state).await?;
    Ok(Json(
        deployments
            .iter()
            .map(|d| deployment_from(d, &repositories))
            .collect(),
    ))
}

async fn create_deployment(
    State(state): State<AppState>,
    Json(request): Json<CreateDeployment>,
) -> Result<(StatusCode, Json<Deployment>), ApiError> {
    let id = deploy::create(&state, request).await?;
    Ok((
        StatusCode::CREATED,
        Json(deployment_view(&state, id).await?),
    ))
}

async fn find_deployment(state: &AppState, key: &str) -> Result<mnix_store::Deployment, ApiError> {
    let not_found = || ApiError::not_found(format!("deployment {key:?}"));
    let id = Uuid::parse_str(key).map_err(|_| not_found())?;
    state
        .store
        .deployment(id)
        .await?
        .filter(|d| d.organization_id == state.organization.id)
        .ok_or_else(not_found)
}

async fn deployment_view(state: &AppState, id: Uuid) -> Result<Deployment, ApiError> {
    let deployment = find_deployment(state, &id.to_string()).await?;
    Ok(deployment_from(
        &deployment,
        &repository_names(state).await?,
    ))
}

async fn get_deployment(
    State(state): State<AppState>,
    Path(deployment): Path<String>,
) -> Result<Json<Deployment>, ApiError> {
    let deployment = find_deployment(&state, &deployment).await?;
    Ok(Json(deployment_from(
        &deployment,
        &repository_names(&state).await?,
    )))
}

#[derive(Debug, Deserialize)]
struct LogQuery {
    after: Option<i64>,
    limit: Option<u32>,
}

async fn deployment_logs(
    State(state): State<AppState>,
    Path(deployment): Path<String>,
    Query(query): Query<LogQuery>,
) -> Result<Json<Vec<LogEntry>>, ApiError> {
    let deployment = find_deployment(&state, &deployment).await?;
    let entries = state
        .store
        .deployment_logs(
            deployment.id,
            query.after.unwrap_or(0),
            query.limit.unwrap_or(500).clamp(1, MAX_LOG_ENTRIES),
        )
        .await?;
    Ok(Json(
        entries
            .into_iter()
            .map(|e| LogEntry {
                id: e.id,
                machine: e.machine_name,
                source: e.source,
                text: e.text,
                created_at: rfc3339(e.created_at),
            })
            .collect(),
    ))
}

async fn signing_key(State(state): State<AppState>) -> Json<SigningKey> {
    Json(SigningKey {
        public_key: state.builder.signing_key.public_key(),
    })
}

async fn create_machine(
    State(state): State<AppState>,
    Json(request): Json<CreateMachine>,
) -> Result<(StatusCode, Json<CreatedMachine>), ApiError> {
    validate_name(&request.name)?;
    let ttl = token_ttl(&state, request.enrollment_token_ttl_secs)?;
    let now = OffsetDateTime::now_utc();
    let machine = state
        .store
        .create_machine(state.organization.id, &request.name, now)
        .await?;
    state
        .event(
            Some(machine.id),
            "machine_created",
            serde_json::json!({ "name": machine.name }),
        )
        .await;
    let enrollment_token = issue_token(&state, machine.id, ttl, now).await?;
    tracing::info!("created machine {} ({})", machine.name, machine.id);
    Ok((
        StatusCode::CREATED,
        Json(CreatedMachine {
            machine: machine_view(
                &machine,
                &HashMap::new(),
                now,
                state.settings.heartbeat_timeout,
            ),
            enrollment_token,
        }),
    ))
}

async fn create_enrollment_token(
    State(state): State<AppState>,
    Path(machine): Path<String>,
    Json(request): Json<CreateEnrollmentToken>,
) -> Result<(StatusCode, Json<EnrollmentToken>), ApiError> {
    let machine = find_machine(&state, &machine).await?;
    let ttl = token_ttl(&state, request.ttl_secs)?;
    let token = issue_token(&state, machine.id, ttl, OffsetDateTime::now_utc()).await?;
    tracing::info!("issued an enrollment token for {}", machine.name);
    Ok((StatusCode::CREATED, Json(token)))
}

/// Looks a machine up by ID or, failing that, by name.
async fn find_machine(state: &AppState, key: &str) -> Result<mnix_store::Machine, ApiError> {
    if let Ok(id) = Uuid::parse_str(key) {
        let found = state.store.machine(id).await?;
        if let Some(machine) = found.filter(|m| m.organization_id == state.organization.id) {
            return Ok(machine);
        }
    }
    state
        .store
        .machines(state.organization.id)
        .await?
        .into_iter()
        .find(|m| m.name == key)
        .ok_or_else(|| ApiError::not_found(format!("machine {key:?}")))
}

async fn issue_token(
    state: &AppState,
    machine_id: Uuid,
    ttl: Duration,
    now: OffsetDateTime,
) -> Result<EnrollmentToken, ApiError> {
    let token = secret::generate(secret::ENROLLMENT_TOKEN_PREFIX);
    let expires_at = now + ttl;
    state
        .store
        .create_enrollment_token(&NewEnrollmentToken {
            machine_id,
            token_hash: secret::hash(&token),
            created_at: now,
            expires_at,
        })
        .await?;
    state
        .event(
            Some(machine_id),
            "enrollment_token_created",
            serde_json::json!({ "expires_at": rfc3339(expires_at) }),
        )
        .await;
    Ok(EnrollmentToken {
        token,
        expires_at: rfc3339(expires_at),
    })
}

fn token_ttl(state: &AppState, secs: Option<u64>) -> Result<Duration, ApiError> {
    let ttl = secs.map_or(state.settings.default_token_ttl, Duration::from_secs);
    if ttl.is_zero() || ttl > MAX_TOKEN_TTL {
        return Err(ApiError::bad_request(format!(
            "enrollment token lifetime must be between 1 second and {}",
            humantime::format_duration(MAX_TOKEN_TTL)
        )));
    }
    Ok(ttl)
}

/// Names of machines, repositories, configurations, and tags look like
/// host names: letters, digits, `-`, `_`, and `.`, starting with a letter or
/// digit, at most 63 characters.
pub fn is_valid_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn validate_name(name: &str) -> Result<(), ApiError> {
    if is_valid_name(name) {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "invalid machine name {name:?}: use up to 63 letters, digits, '-', '_', or '.', \
             starting with a letter or digit"
        )))
    }
}

pub fn status(
    machine: &mnix_store::Machine,
    now: OffsetDateTime,
    timeout: Duration,
) -> MachineStatus {
    if machine.enrolled_at.is_none() {
        return MachineStatus::Pending;
    }
    let fresh = machine
        .last_seen_at
        .is_some_and(|seen| now - seen <= timeout);
    if machine.connected && fresh {
        MachineStatus::Online
    } else {
        MachineStatus::Offline
    }
}

fn repository_view(repository: &mnix_store::Repository) -> Repository {
    Repository {
        id: repository.id.to_string(),
        name: repository.name.clone(),
        url: repository.url.clone(),
        created_at: rfc3339(repository.created_at),
    }
}

fn deployment_from(
    deployment: &mnix_store::Deployment,
    repositories: &HashMap<Uuid, String>,
) -> Deployment {
    Deployment {
        id: deployment.id.to_string(),
        kind: deployment.kind,
        status: deployment.status,
        message: deployment.message.clone(),
        repository: deployment
            .repository_id
            .and_then(|id| repositories.get(&id).cloned()),
        rev: deployment.rev.clone(),
        commit: deployment.commit_id.clone(),
        mode: deployment.mode,
        requested_by: deployment.requested_by.clone(),
        created_at: rfc3339(deployment.created_at),
        started_at: deployment.started_at.map(rfc3339),
        finished_at: deployment.finished_at.map(rfc3339),
        machines: deployment
            .machines
            .iter()
            .map(|m| DeploymentMachine {
                machine_id: m.machine_id.to_string(),
                machine: m.machine_name.clone(),
                configuration: m.configuration.clone(),
                status: m.status,
                phase: m.phase,
                store_path: m.store_path.clone(),
                message: m.message.clone(),
                report: m.report.clone(),
                finished_at: m.finished_at.map(rfc3339),
            })
            .collect(),
    }
}

fn machine_view(
    machine: &mnix_store::Machine,
    repositories: &HashMap<Uuid, String>,
    now: OffsetDateTime,
    timeout: Duration,
) -> Machine {
    Machine {
        id: machine.id.to_string(),
        name: machine.name.clone(),
        status: status(machine, now, timeout),
        created_at: rfc3339(machine.created_at),
        enrolled_at: machine.enrolled_at.map(rfc3339),
        last_seen_at: machine.last_seen_at.map(rfc3339),
        agent_version: machine.agent_version.clone(),
        current_system: machine.facts.as_ref().map(|f| f.current_system.clone()),
        facts: machine.facts.clone(),
        repository: machine
            .repository_id
            .and_then(|id| repositories.get(&id).cloned()),
        configuration: machine.configuration.clone(),
        tags: machine.tags.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_must_look_like_host_names() {
        for ok in ["web-1", "db.internal", "a", "Node_2", &"x".repeat(63)] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-web", ".web", "web 1", "wéb", "a/b", &"x".repeat(64)] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn git_urls_exclude_transport_helpers_and_options() {
        for ok in [
            "https://github.com/me/infra.git",
            "ssh://git@host/infra",
            "git@github.com:me/infra.git",
            "file:///srv/git/infra",
            "/srv/git/infra",
        ] {
            assert!(validate_git_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "ext::sh -c touch% /tmp/pwned",
            "fd::17",
            "-uhttps://x",
            "relative/path",
            "https://host/a b",
        ] {
            assert!(validate_git_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn status_depends_on_enrollment_session_and_heartbeat() {
        let now = OffsetDateTime::now_utc();
        let timeout = Duration::from_secs(30);
        let mut m = mnix_store::Machine {
            id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
            name: "web-1".to_owned(),
            created_at: now,
            enrolled_at: None,
            agent_version: None,
            facts: None,
            facts_updated_at: None,
            last_seen_at: None,
            connected: false,
            repository_id: None,
            configuration: None,
            tags: Vec::new(),
        };
        assert_eq!(status(&m, now, timeout), MachineStatus::Pending);
        m.enrolled_at = Some(now);
        assert_eq!(status(&m, now, timeout), MachineStatus::Offline);
        m.connected = true;
        m.last_seen_at = Some(now - time::Duration::seconds(10));
        assert_eq!(status(&m, now, timeout), MachineStatus::Online);
        m.last_seen_at = Some(now - time::Duration::seconds(31));
        assert_eq!(status(&m, now, timeout), MachineStatus::Offline);
    }
}
