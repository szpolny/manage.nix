//! `GET /api/deployments/:id/events`: a deployment's progress as
//! server-sent events, for the web UI.
//!
//! Events:
//! - `deployment`: the whole [`Deployment`](mnix_protocol::api::Deployment),
//!   first on connecting and then whenever it changes;
//! - `log`: one [`LogEntry`](mnix_protocol::api::LogEntry), with the entry's
//!   ID as the event ID, so a reconnecting `EventSource` resumes after the
//!   last entry it saw (`Last-Event-ID`);
//! - `end`: the deployment finished and every log entry was sent; the
//!   stream closes.
//!
//! The stream polls the database, so it follows deployments built and
//! activated by any server replica.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use serde::Deserialize;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::api::{deployment_view, find_deployment, log_entry_view};
use crate::app::{ApiError, State as AppState};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const LOG_BATCH: u32 = 500;

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    /// Send log entries after this ID (like `Last-Event-ID`).
    after: Option<i64>,
}

pub async fn deployment_events(
    State(state): State<AppState>,
    Path(deployment): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let deployment = find_deployment(&state, &deployment).await?;
    let after = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .or(query.after)
        .unwrap_or(0);
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(follow(state, deployment.id, after, tx));
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (Ok(event), rx))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Sends events until the deployment finishes or the client goes away.
async fn follow(state: AppState, id: Uuid, mut after: i64, tx: mpsc::Sender<Event>) {
    let mut last = None;
    let mut interval = tokio::time::interval(POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            () = tx.closed() => return,
        }
        let deployment = match deployment_view(&state, id).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("following deployment {id}: {}", e.message);
                return;
            }
        };
        let finished = deployment.status.is_finished();
        if last.as_ref() != Some(&deployment) {
            let event = Event::default()
                .event("deployment")
                .json_data(&deployment)
                .expect("deployments serialize");
            if tx.send(event).await.is_err() {
                return;
            }
            last = Some(deployment);
        }
        // Drain the log; a finished deployment gets no new entries, so
        // once it is drained the stream is complete.
        loop {
            let entries = match state.store.deployment_logs(id, after, LOG_BATCH).await {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::error!("reading the log of deployment {id}: {e}");
                    return;
                }
            };
            let more = entries.len() == LOG_BATCH as usize;
            for entry in entries {
                after = entry.id;
                let event = Event::default()
                    .event("log")
                    .id(entry.id.to_string())
                    .json_data(log_entry_view(entry))
                    .expect("log entries serialize");
                if tx.send(event).await.is_err() {
                    return;
                }
            }
            if !more {
                break;
            }
        }
        if finished {
            let _ = tx.send(Event::default().event("end").data("")).await;
            return;
        }
    }
}
