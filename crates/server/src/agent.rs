//! The agent-facing endpoints: enrollment and the WebSocket session.

use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap};
use axum::response::Response;
use axum::Json;
use mnix_protocol::api::{EnrollRequest, EnrollResponse};
use mnix_protocol::{AgentMessage, Hello, ServerMessage, PROTOCOL_VERSION};
use mnix_store::{disconnect_reason, Machine, NewSession, Redemption};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::app::{ApiError, AppState, State as AppStateRef};
use crate::secret;

/// The largest WebSocket message the server accepts from an agent.
const MAX_MESSAGE_SIZE: usize = 1 << 20;

/// `POST /api/agent/enroll`: exchange a one-time token for a credential.
pub async fn enroll(
    State(state): State<AppStateRef>,
    Json(request): Json<EnrollRequest>,
) -> Result<Json<EnrollResponse>, ApiError> {
    let credential = secret::generate(secret::CREDENTIAL_PREFIX);
    let redemption = state
        .store
        .redeem_enrollment_token(
            &secret::hash(&request.token),
            &secret::hash(&credential),
            OffsetDateTime::now_utc(),
        )
        .await?;
    let (machine_id, reason) = match redemption {
        Redemption::Enrolled { machine_id } => {
            tracing::info!("machine {machine_id} enrolled");
            state
                .event(Some(machine_id), "machine_enrolled", serde_json::json!({}))
                .await;
            return Ok(Json(EnrollResponse {
                machine_id: machine_id.to_string(),
                credential,
            }));
        }
        Redemption::Unknown => (None, "unknown"),
        Redemption::AlreadyUsed { machine_id } => (Some(machine_id), "already_used"),
        Redemption::Expired { machine_id } => (Some(machine_id), "expired"),
    };
    tracing::warn!("rejected an enrollment ({reason} token)");
    if machine_id.is_some() {
        state
            .event(
                machine_id,
                "enrollment_rejected",
                serde_json::json!({ "reason": reason }),
            )
            .await;
    }
    // The client learns only that the token is not valid now.
    Err(ApiError::unauthorized(
        "invalid, already used, or expired enrollment token",
    ))
}

/// `GET /api/agent/ws`: authenticate the machine, then run its session.
pub async fn connect(
    State(state): State<AppStateRef>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let credential = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(secret::bearer)
        .ok_or_else(|| ApiError::unauthorized("missing machine credential"))?;
    let machine = state
        .store
        .machine_by_credential(&secret::hash(credential))
        .await?
        .filter(|m| m.organization_id == state.organization.id)
        .ok_or_else(|| ApiError::unauthorized("unknown machine credential"))?;
    Ok(ws
        .max_message_size(MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| async move {
            Session::run(&state, machine, socket).await;
        }))
}

struct Session<'a> {
    state: &'a AppState,
    machine: Machine,
    socket: WebSocket,
}

/// What ended a session.
enum End {
    /// Close with this reason; the agent did nothing wrong.
    Reason(&'static str),
    /// The agent broke the protocol; tell it why.
    ProtocolError(String),
}

impl Session<'_> {
    async fn run(state: &AppState, machine: Machine, socket: WebSocket) {
        let mut session = Session {
            state,
            machine,
            socket,
        };
        let name = session.machine.name.clone();
        let hello = match session.receive_hello().await {
            Ok(hello) => hello,
            Err(end) => {
                tracing::info!("{name}: no valid hello, closing");
                session.close(end).await;
                return;
            }
        };
        let session_id = match session.open(hello).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("{name}: opening session: {e}");
                session.close(End::Reason(disconnect_reason::CLOSED)).await;
                return;
            }
        };
        tracing::info!("{name}: connected (session {session_id})");
        let end = session.serve(session_id).await;
        let reason = match &end {
            End::Reason(reason) => *reason,
            End::ProtocolError(_) => disconnect_reason::PROTOCOL_ERROR,
        };
        tracing::info!("{name}: disconnected ({reason})");
        let machine_id = session.machine.id;
        session.close(end).await;
        match state
            .store
            .close_session(session_id, reason, OffsetDateTime::now_utc())
            .await
        {
            // If it was already closed (replaced or reaped), whoever closed
            // it recorded why.
            Ok(true) => {
                state
                    .event(
                        Some(machine_id),
                        "agent_disconnected",
                        serde_json::json!({ "session_id": session_id, "reason": reason }),
                    )
                    .await;
            }
            Ok(false) => {}
            Err(e) => tracing::error!("{name}: closing session {session_id}: {e}"),
        }
    }

    fn timeout(&self) -> Duration {
        self.state.settings.heartbeat_timeout
    }

    async fn receive_hello(&mut self) -> Result<Hello, End> {
        let message = tokio::time::timeout(self.timeout(), self.next_message())
            .await
            .map_err(|_| End::Reason(disconnect_reason::HEARTBEAT_TIMEOUT))??;
        let AgentMessage::Hello(hello) = message else {
            return Err(End::ProtocolError("expected hello".to_owned()));
        };
        if hello.protocol_version != PROTOCOL_VERSION {
            return Err(End::ProtocolError(format!(
                "unsupported protocol version {}; this server speaks {PROTOCOL_VERSION}",
                hello.protocol_version
            )));
        }
        Ok(hello)
    }

    async fn open(&mut self, hello: Hello) -> anyhow::Result<Uuid> {
        let session_id = self
            .state
            .store
            .open_session(&NewSession {
                machine_id: self.machine.id,
                agent_version: hello.agent_version.clone(),
                facts: hello.facts,
                connected_at: OffsetDateTime::now_utc(),
            })
            .await?;
        self.state
            .event(
                Some(self.machine.id),
                "agent_connected",
                serde_json::json!({
                    "session_id": session_id,
                    "agent_version": hello.agent_version,
                }),
            )
            .await;
        let settings = &self.state.settings;
        self.send(&ServerMessage::Welcome {
            protocol_version: PROTOCOL_VERSION,
            machine_id: self.machine.id.to_string(),
            heartbeat_interval_secs: settings.heartbeat_interval.as_secs(),
            heartbeat_timeout_secs: settings.heartbeat_timeout.as_secs(),
        })
        .await?;
        Ok(session_id)
    }

    async fn serve(&mut self, session_id: Uuid) -> End {
        let mut shutdown = self.state.shutdown.clone();
        loop {
            let message = tokio::select! {
                message = tokio::time::timeout(self.timeout(), self.next_message()) => message,
                _ = shutdown.wait_for(|stopping| *stopping) => {
                    return End::Reason(disconnect_reason::SERVER_SHUTDOWN);
                }
            };
            let message = match message {
                Err(_) => return End::Reason(disconnect_reason::HEARTBEAT_TIMEOUT),
                Ok(Err(end)) => return end,
                Ok(Ok(message)) => message,
            };
            let now = OffsetDateTime::now_utc();
            let store = &self.state.store;
            let result = match message {
                AgentMessage::Heartbeat => store.heartbeat(session_id, now).await,
                AgentMessage::Facts { facts } => {
                    match store.update_facts(self.machine.id, &facts, now).await {
                        Ok(()) => store.heartbeat(session_id, now).await,
                        Err(e) => Err(e),
                    }
                }
                AgentMessage::Hello(_) => {
                    return End::ProtocolError("unexpected hello".to_owned());
                }
            };
            match result {
                Ok(true) => {}
                // Another session replaced this one, or the reaper closed it.
                Ok(false) => return End::Reason(disconnect_reason::REPLACED),
                Err(e) => {
                    tracing::error!("{}: {e}", self.machine.name);
                    return End::Reason(disconnect_reason::CLOSED);
                }
            }
        }
    }

    /// The next protocol message, skipping control frames and messages of
    /// unknown types.
    async fn next_message(&mut self) -> Result<AgentMessage, End> {
        loop {
            let frame = match self.socket.recv().await {
                Some(Ok(frame)) => frame,
                None | Some(Err(_)) => return Err(End::Reason(disconnect_reason::CLOSED)),
            };
            match frame {
                Message::Text(text) => match parse(&text) {
                    Ok(Some(message)) => return Ok(message),
                    Ok(None) => {}
                    Err(e) => return Err(End::ProtocolError(e)),
                },
                Message::Binary(_) => {
                    return Err(End::ProtocolError(
                        "binary messages are not supported".into(),
                    ))
                }
                Message::Close(_) => return Err(End::Reason(disconnect_reason::CLOSED)),
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
    }

    async fn send(&mut self, message: &ServerMessage) -> anyhow::Result<()> {
        let text = serde_json::to_string(message)?;
        self.socket.send(Message::Text(text.into())).await?;
        Ok(())
    }

    async fn close(&mut self, end: End) {
        let (code, reason) = match end {
            End::Reason(disconnect_reason::SERVER_SHUTDOWN) => (1001, "server shutting down"),
            End::Reason(_) => (1000, ""),
            End::ProtocolError(message) => {
                tracing::warn!("{}: protocol error: {message}", self.machine.name);
                let _ = self.send(&ServerMessage::Error { message }).await;
                (1008, "protocol error")
            }
        };
        let _ = self
            .socket
            .send(Message::Close(Some(CloseFrame {
                code,
                reason: Utf8Bytes::from_static(reason),
            })))
            .await;
    }
}

/// Parses an agent message. Unknown message types are logged and skipped
/// (`Ok(None)`) so newer agents can add messages; malformed known messages
/// are errors.
fn parse(text: &str) -> Result<Option<AgentMessage>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let kind = value
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or("message has no type")?
        .to_owned();
    match serde_json::from_value(value) {
        Ok(message) => Ok(Some(message)),
        Err(_) if !matches!(kind.as_str(), "hello" | "heartbeat" | "facts") => {
            tracing::debug!("ignoring unknown agent message {kind:?}");
            Ok(None)
        }
        Err(e) => Err(format!("invalid {kind} message: {e}")),
    }
}

/// Closes sessions whose heartbeats stopped, for connections this server
/// instance cannot see failing (another replica's, or ones that died with a
/// previous process).
pub async fn reap_stale_sessions(state: AppStateRef) {
    let mut interval = tokio::time::interval(state.settings.heartbeat_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let now = OffsetDateTime::now_utc();
        let closed = state
            .store
            .close_stale_sessions(
                now - state.settings.heartbeat_timeout,
                disconnect_reason::HEARTBEAT_TIMEOUT,
                now,
            )
            .await;
        match closed {
            Ok(closed) => {
                for session in closed {
                    tracing::info!("machine {}: heartbeat timeout", session.machine_id);
                    state
                        .event(
                            Some(session.machine_id),
                            "agent_disconnected",
                            serde_json::json!({
                                "session_id": session.session_id,
                                "reason": disconnect_reason::HEARTBEAT_TIMEOUT,
                            }),
                        )
                        .await;
                }
            }
            Err(e) => tracing::error!("closing stale sessions: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_message_types_are_skipped() {
        assert_eq!(
            parse(r#"{"type": "heartbeat"}"#),
            Ok(Some(AgentMessage::Heartbeat))
        );
        assert_eq!(parse(r#"{"type": "from_the_future", "x": 1}"#), Ok(None));
        assert!(parse(r#"{"type": "facts"}"#).is_err());
        assert!(parse(r#"{"no": "type"}"#).is_err());
        assert!(parse("not json").is_err());
    }
}
