//! The admin HTTP API used by `mnix` (and later the web UI). Every route
//! requires `Authorization: Bearer <admin token>`.

use std::time::Duration;

use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use mnix_protocol::api::{
    CreateEnrollmentToken, CreateMachine, CreatedMachine, EnrollmentToken, Machine, MachineStatus,
};
use mnix_store::NewEnrollmentToken;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::app::{rfc3339, ApiError, State as AppState};
use crate::secret;

/// The longest an enrollment token may be valid.
const MAX_TOKEN_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

pub fn routes(state: AppState) -> axum::Router<AppState> {
    Router::new()
        .route("/api/machines", get(list_machines).post(create_machine))
        .route("/api/machines/{machine}", get(get_machine))
        .route(
            "/api/machines/{machine}/enrollment-tokens",
            post(create_enrollment_token),
        )
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

async fn list_machines(State(state): State<AppState>) -> Result<Json<Vec<Machine>>, ApiError> {
    let now = OffsetDateTime::now_utc();
    let machines = state.store.machines(state.organization.id).await?;
    Ok(Json(
        machines
            .iter()
            .map(|m| machine_view(m, now, state.settings.heartbeat_timeout))
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
        OffsetDateTime::now_utc(),
        state.settings.heartbeat_timeout,
    )))
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
            machine: machine_view(&machine, now, state.settings.heartbeat_timeout),
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

/// Machine names look like host names: letters, digits, `-`, `_`, and `.`,
/// starting with a letter or digit, at most 63 characters.
fn validate_name(name: &str) -> Result<(), ApiError> {
    let valid = (1..=63).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if valid {
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

fn machine_view(machine: &mnix_store::Machine, now: OffsetDateTime, timeout: Duration) -> Machine {
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
