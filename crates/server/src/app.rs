//! The HTTP application: shared state, routes, and error responses.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mnix_protocol::api::ErrorBody;
use mnix_store::{Event, Organization, Store};
use time::OffsetDateTime;
use tokio::sync::watch;
use uuid::Uuid;

use crate::{agent, api};

/// Settings that shape the API's behaviour.
#[derive(Debug, Clone)]
pub struct Settings {
    pub heartbeat_interval: Duration,
    pub heartbeat_timeout: Duration,
    pub default_token_ttl: Duration,
}

pub struct AppState {
    pub store: Arc<dyn Store>,
    /// M2 has one organization; every machine belongs to it.
    pub organization: Organization,
    /// SHA-256 (hex) of the admin token.
    pub admin_token_hash: String,
    pub settings: Settings,
    /// Becomes true when the server is shutting down, so agent sessions
    /// close cleanly.
    pub shutdown: watch::Receiver<bool>,
}

pub type State = Arc<AppState>;

impl AppState {
    /// Records an event, logging (not returning) failures: losing an event
    /// must not fail the request that caused it.
    pub async fn event(&self, machine_id: Option<Uuid>, kind: &str, data: serde_json::Value) {
        let event = Event {
            id: Uuid::new_v4(),
            organization_id: self.organization.id,
            machine_id,
            kind: kind.to_owned(),
            data,
            created_at: OffsetDateTime::now_utc(),
        };
        if let Err(e) = self.store.record_event(&event).await {
            tracing::error!("recording {kind} event: {e}");
        }
    }
}

pub fn router(state: State) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api::routes(state.clone()))
        .route(mnix_protocol::ENROLL_PATH, post(agent::enroll))
        .route(mnix_protocol::AGENT_WS_PATH, get(agent::connect))
        .with_state(state)
}

/// API timestamps: RFC 3339 in UTC, to the second (`2026-10-22T08:00:00Z`).
pub fn rfc3339(t: OffsetDateTime) -> String {
    t.to_offset(time::UtcOffset::UTC)
        .format(time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second]Z"
        ))
        .expect("UTC timestamps always format")
}

/// An error response with an [`ErrorBody`].
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn not_found(what: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::NOT_FOUND, format!("{what} not found"))
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }
}

impl From<mnix_store::Error> for ApiError {
    fn from(e: mnix_store::Error) -> Self {
        match e {
            mnix_store::Error::Conflict(what) => {
                Self::new(StatusCode::CONFLICT, format!("{what} already exists"))
            }
            e => {
                tracing::error!("database error: {e}");
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamps_are_utc_to_the_second() {
        let t = time::macros::datetime!(2026-10-22 10:00:00.5 +02:00);
        assert_eq!(super::rfc3339(t), "2026-10-22T08:00:00Z");
    }
}
