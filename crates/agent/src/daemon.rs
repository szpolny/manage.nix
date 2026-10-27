//! `mnix-agent run`: enroll if needed, then keep one outbound WebSocket to
//! the server open, reconnecting with backoff.
//!
//! The server's `activate` requests become jobs ([`crate::job`]) that run
//! outside this service. The session reports their progress, logs, and
//! results, and passes the server's `confirm` on. A job waiting for
//! confirmation must be confirmed over a connection opened *after* it
//! started waiting: an existing connection can outlive a configuration that
//! breaks networking (the firewall still lets established connections
//! through), so the session reconnects first.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use mnix_protocol::{
    Activate, ActivationPhase, ActivationReport, ActivationStatus, ActivationTarget, AgentMessage,
    Facts, Hello, ServerMessage, PROTOCOL_VERSION,
};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::credential::{self, Credential, EnrollError};
use crate::facts;
use crate::job::{Job, Jobs};

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The longest wait between reconnection attempts while an activation is
/// running: its confirmation window is short.
const ACTIVE_JOB_BACKOFF: Duration = Duration::from_secs(2);
/// How long to wait for the server's `welcome` after connecting.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the session looks at activation jobs.
const JOB_POLL: Duration = Duration::from_secs(1);
/// The largest piece of activation log sent in one message.
const LOG_CHUNK: usize = 32 * 1024;

pub struct Options {
    /// The server's base URL (`https://...`).
    pub server_url: String,
    pub enrollment_token_file: Option<PathBuf>,
    pub state_dir: PathBuf,
}

pub async fn run(options: Options) -> anyhow::Result<()> {
    let server_url = options.server_url.trim_end_matches('/').to_owned();
    let ws_url = websocket_url(&server_url)?;
    if ws_url.starts_with("ws://") {
        tracing::warn!("{server_url} is not HTTPS; the credential is sent unencrypted");
    }

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut backoff = Backoff::default();

    let credential = loop {
        let attempt = enroll_or_load(&options, &server_url);
        tokio::select! {
            result = attempt => match result {
                Ok(credential) => break credential,
                Err(e) => tracing::error!("{e:#}"),
            },
            _ = &mut shutdown => return Ok(()),
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff.next()) => {}
            _ = &mut shutdown => return Ok(()),
        }
    };
    if credential.server_url != server_url {
        tracing::warn!(
            "enrolled with {}, now configured for {server_url}; using the stored credential",
            credential.server_url
        );
    }
    tracing::info!("machine {}: connecting to {ws_url}", credential.machine_id);

    let jobs = Jobs::new(&options.state_dir);
    let mut backoff = Backoff::default();
    loop {
        let mut session = Session::new(&ws_url, &credential, &options.state_dir, &jobs);
        tokio::select! {
            result = session.run() => {
                let reconnect = match result {
                    Ok(Ended::Closed) => {
                        tracing::info!("server closed the connection");
                        false
                    }
                    Ok(Ended::Reconnect) => {
                        tracing::info!(
                            "an activation is waiting for confirmation; reconnecting to show \
                             that the new system reaches the server"
                        );
                        true
                    }
                    Err(e) => {
                        tracing::warn!("connection failed: {e:#}");
                        false
                    }
                };
                session.close().await;
                if session.welcomed {
                    backoff = Backoff::default();
                }
                if reconnect {
                    continue;
                }
            }
            _ = &mut shutdown => {
                session.close().await;
                return Ok(());
            }
        }
        let mut delay = backoff.next();
        if list_jobs(&jobs)
            .await
            .iter()
            .any(|job| job.report.is_none())
        {
            delay = delay.min(ACTIVE_JOB_BACKOFF);
        }
        tracing::info!("reconnecting in {}", humantime::format_duration(delay));
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = &mut shutdown => return Ok(()),
        }
    }
}

/// The stored credential, or a new one from the enrollment token.
async fn enroll_or_load(options: &Options, server_url: &str) -> anyhow::Result<Credential> {
    if let Some(credential) = credential::load(&options.state_dir)? {
        return Ok(credential);
    }
    let token_file = options.enrollment_token_file.as_deref().with_context(|| {
        format!(
            "not enrolled: no {} and no enrollment token file configured",
            credential::path(&options.state_dir).display()
        )
    })?;
    let token = credential::read_token(token_file)?;
    tracing::info!("enrolling with {server_url}");
    let url = server_url.to_owned();
    let credential = tokio::task::spawn_blocking(move || credential::enroll(&url, &token))
        .await?
        .map_err(|e| match e {
            EnrollError::Rejected(why) => anyhow::anyhow!(
                "enrollment failed: {why}. Create a new token with `mnix machine token <name>` \
                 and write it to {}",
                token_file.display()
            ),
            EnrollError::Other(e) => e.context("enrollment failed"),
        })?;
    credential::store(&options.state_dir, &credential)?;
    tracing::info!("enrolled as machine {}", credential.machine_id);
    Ok(credential)
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// How a session ended, when it did not fail.
enum Ended {
    /// The server closed the connection.
    Closed,
    /// A job needs confirmation over a new connection.
    Reconnect,
}

struct Session<'a> {
    ws_url: &'a str,
    credential: &'a Credential,
    credential_path: PathBuf,
    jobs: &'a Jobs,
    socket: Option<Socket>,
    /// Whether the server accepted the `hello`.
    welcomed: bool,
}

/// What this session has told the server about one job.
#[derive(Debug, Default)]
struct Reported {
    phase: Option<ActivationPhase>,
    /// How much of the job's log has been sent.
    log_offset: u64,
    result: bool,
}

impl<'a> Session<'a> {
    fn new(ws_url: &'a str, credential: &'a Credential, state_dir: &Path, jobs: &'a Jobs) -> Self {
        Self {
            ws_url,
            credential,
            credential_path: credential::path(state_dir),
            jobs,
            socket: None,
            welcomed: false,
        }
    }

    async fn run(&mut self) -> anyhow::Result<Ended> {
        let mut request = self.ws_url.into_client_request()?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", self.credential.credential).parse()?,
        );
        let socket = match tokio_tungstenite::connect_async(request).await {
            Ok((socket, _)) => self.socket.insert(socket),
            Err(WsError::Http(response)) if response.status() == StatusCode::UNAUTHORIZED => {
                anyhow::bail!(
                    "the server rejected this machine's credential; remove {} and enroll again",
                    self.credential_path.display()
                )
            }
            Err(e) => return Err(e).context("connecting"),
        };

        let mut facts = collect_facts().await?;
        send(
            socket,
            &AgentMessage::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                agent_version: env!("CARGO_PKG_VERSION").to_owned(),
                facts: facts.clone(),
            }),
        )
        .await?;
        let welcome = tokio::time::timeout(WELCOME_TIMEOUT, async {
            loop {
                match receive(socket).await? {
                    Incoming::Message(message) => return Ok(Some(message)),
                    Incoming::Alive => {}
                    Incoming::Closed => return anyhow::Ok(None),
                }
            }
        });
        let (interval, timeout) = match welcome.await.context("no welcome from the server")?? {
            Some(ServerMessage::Welcome {
                heartbeat_interval_secs,
                heartbeat_timeout_secs,
                ..
            }) => (
                Duration::from_secs(heartbeat_interval_secs.max(1)),
                Duration::from_secs(heartbeat_timeout_secs.max(heartbeat_interval_secs + 1)),
            ),
            Some(ServerMessage::Error { message }) => anyhow::bail!("server: {message}"),
            Some(other) => anyhow::bail!("expected welcome, got {other:?}"),
            None => anyhow::bail!("connection closed before welcome"),
        };
        self.welcomed = true;
        let connected_at = SystemTime::now();
        tracing::info!(
            "connected; heartbeat every {}",
            humantime::format_duration(interval)
        );

        let mut reported = HashMap::new();
        let mut ticker = tokio::time::interval_at(Instant::now() + interval, interval);
        let mut job_ticker = tokio::time::interval(JOB_POLL);
        let mut last_heard = Instant::now();
        loop {
            tokio::select! {
                _ = job_ticker.tick() => {
                    let jobs = list_jobs(self.jobs).await;
                    if report_jobs(socket, &jobs, &mut reported, connected_at, &mut facts).await? {
                        return Ok(Ended::Reconnect);
                    }
                }
                _ = ticker.tick() => {
                    // A server that stopped answering pings is gone even if
                    // the TCP connection looks open.
                    anyhow::ensure!(
                        last_heard.elapsed() < timeout,
                        "nothing from the server for {}",
                        humantime::format_duration(timeout)
                    );
                    let current = collect_facts().await?;
                    let message = if current == facts {
                        AgentMessage::Heartbeat
                    } else {
                        facts = current.clone();
                        AgentMessage::Facts { facts: current }
                    };
                    send(socket, &message).await?;
                    socket.send(Message::Ping(Default::default())).await?;
                }
                message = receive(socket) => {
                    last_heard = Instant::now();
                    match message? {
                        Incoming::Message(ServerMessage::Error { message }) => {
                            tracing::error!("server: {message}");
                        }
                        Incoming::Message(ServerMessage::Activate(request)) => {
                            start_job(socket, self.jobs, request).await?;
                            job_ticker.reset_immediately();
                        }
                        Incoming::Message(ServerMessage::Confirm { deployment_id }) => {
                            let jobs = list_jobs(self.jobs).await;
                            confirm_job(self.jobs, &jobs, &deployment_id, connected_at);
                            job_ticker.reset_immediately();
                        }
                        Incoming::Message(ServerMessage::ResultReceived { deployment_id }) => {
                            reported.remove(&deployment_id);
                            if let Err(e) = self.jobs.remove(&deployment_id) {
                                tracing::error!("{e:#}");
                            }
                        }
                        Incoming::Message(ServerMessage::Welcome { .. }) | Incoming::Alive => {}
                        Incoming::Closed => return Ok(Ended::Closed),
                    }
                }
            }
        }
    }

    /// Closes the connection cleanly, so the server marks the machine
    /// offline at once instead of after the heartbeat timeout.
    async fn close(&mut self) {
        if let Some(socket) = &mut self.socket {
            let frame = CloseFrame {
                code: CloseCode::Away,
                reason: "agent stopping".into(),
            };
            let _ = tokio::time::timeout(Duration::from_secs(2), socket.close(Some(frame))).await;
        }
    }
}

async fn list_jobs(jobs: &Jobs) -> Vec<Job> {
    let jobs = jobs.clone();
    tokio::task::spawn_blocking(move || jobs.list())
        .await
        .unwrap_or_default()
}

/// Starts a job for the server's request (unless it is already known).
async fn start_job(socket: &mut Socket, jobs: &Jobs, request: Activate) -> anyhow::Result<()> {
    let jobs = jobs.clone();
    let deployment_id = request.deployment_id.clone();
    let started = tokio::task::spawn_blocking({
        let request = request.clone();
        move || jobs.start(&request)
    })
    .await?;
    match started {
        Ok(true) => tracing::info!("deployment {deployment_id}: starting the activation"),
        Ok(false) => tracing::debug!("deployment {deployment_id}: already known"),
        // The request could not be stored (an invalid ID); answer at once.
        Err(e) => {
            tracing::error!("deployment {deployment_id}: {e:#}");
            let report = ActivationReport {
                store_path: match request.target {
                    ActivationTarget::System { store_path } => store_path,
                    ActivationTarget::PreviousGeneration => String::new(),
                },
                mode: request.mode,
                status: ActivationStatus::Failed,
                system_changed: false,
                previous_system: None,
                message: Some(format!("refused: {e:#}")),
                failed_checks: Vec::new(),
            };
            send(
                socket,
                &AgentMessage::ActivationResult {
                    deployment_id,
                    report,
                },
            )
            .await?;
        }
    }
    Ok(())
}

/// Passes the server's confirmation on, if the job is waiting for it and
/// this session began after it started waiting.
fn confirm_job(jobs: &Jobs, listed: &[Job], deployment_id: &str, connected_at: SystemTime) {
    let waiting = listed
        .iter()
        .find(|job| job.deployment_id == deployment_id)
        .and_then(Job::awaiting_since);
    match waiting {
        Some(since) if since < connected_at => match jobs.confirm(deployment_id) {
            Ok(()) => tracing::info!("deployment {deployment_id}: confirmed by the server"),
            Err(e) => tracing::error!("{e:#}"),
        },
        Some(_) => tracing::warn!(
            "deployment {deployment_id}: ignoring a confirmation over a connection opened \
             before the switch"
        ),
        None => tracing::warn!("deployment {deployment_id}: ignoring an unexpected confirmation"),
    }
}

/// Sends what the server has not heard from this session about each job:
/// new log output, a new phase, or the result. Returns true if a job is
/// waiting for confirmation over a newer connection than this one.
async fn report_jobs(
    socket: &mut Socket,
    jobs: &[Job],
    reported: &mut HashMap<String, Reported>,
    connected_at: SystemTime,
    facts: &mut Facts,
) -> anyhow::Result<bool> {
    for job in jobs {
        let seen = reported.entry(job.deployment_id.clone()).or_default();
        for (offset, text) in read_log(job, seen.log_offset, job.report.is_some()).await {
            seen.log_offset = offset + text.len() as u64;
            send(
                socket,
                &AgentMessage::ActivationLog {
                    deployment_id: job.deployment_id.clone(),
                    offset,
                    text,
                },
            )
            .await?;
        }
        if let Some(report) = &job.report {
            if !seen.result {
                // Let the server see the new system along with the result.
                let current = collect_facts().await?;
                if current != *facts {
                    *facts = current.clone();
                    send(socket, &AgentMessage::Facts { facts: current }).await?;
                }
                send(
                    socket,
                    &AgentMessage::ActivationResult {
                        deployment_id: job.deployment_id.clone(),
                        report: report.clone(),
                    },
                )
                .await?;
                seen.result = true;
            }
            continue;
        }
        if job
            .awaiting_since()
            .is_some_and(|since| since >= connected_at)
        {
            return Ok(true);
        }
        let phase = job.phase.unwrap_or(ActivationPhase::Accepted);
        if seen.phase != Some(phase) {
            send(
                socket,
                &AgentMessage::ActivationStatus {
                    deployment_id: job.deployment_id.clone(),
                    phase,
                },
            )
            .await?;
            seen.phase = Some(phase);
        }
    }
    Ok(false)
}

/// The job's log from `offset`, in pieces of whole lines (the rest too if
/// `all`). The server keeps the bytes it has not seen yet, by offset.
async fn read_log(job: &Job, offset: u64, all: bool) -> Vec<(u64, String)> {
    let path = job.log_path();
    let bytes = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
        use std::io::{Read, Seek};
        let mut file = std::fs::File::open(path)?;
        file.seek(std::io::SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
    .await;
    let Ok(Ok(bytes)) = bytes else {
        return Vec::new();
    };
    chunk_log(&bytes, offset, all)
}

/// Splits log bytes into messages of at most [`LOG_CHUNK`] bytes, ending at
/// line breaks unless a line is longer than that (or `all` is set for the
/// final piece). Each text has the same length in bytes as the log bytes
/// it stands for, so offsets stay exact.
fn chunk_log(mut bytes: &[u8], mut offset: u64, all: bool) -> Vec<(u64, String)> {
    let mut chunks = Vec::new();
    while !bytes.is_empty() {
        let window = &bytes[..bytes.len().min(LOG_CHUNK)];
        let len = match window.iter().rposition(|&b| b == b'\n') {
            Some(newline) => newline + 1,
            None if window.len() == LOG_CHUNK || all => window.len(),
            None => break,
        };
        chunks.push((offset, sanitize(&bytes[..len])));
        offset += len as u64;
        bytes = &bytes[len..];
    }
    chunks
}

/// Text for log bytes: invalid UTF-8 becomes `?` and NUL a space (Postgres
/// text cannot hold it), byte for byte.
fn sanitize(mut bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    loop {
        match std::str::from_utf8(bytes) {
            Ok(valid) => {
                out.push_str(valid);
                break;
            }
            Err(e) => {
                let (valid, rest) = bytes.split_at(e.valid_up_to());
                out.push_str(std::str::from_utf8(valid).expect("checked above"));
                let bad = e.error_len().unwrap_or(rest.len());
                out.extend(std::iter::repeat_n('?', bad));
                bytes = &rest[bad..];
            }
        }
    }
    out.replace('\0', " ")
}

async fn collect_facts() -> anyhow::Result<Facts> {
    tokio::task::spawn_blocking(facts::collect)
        .await?
        .context("collecting facts")
}

async fn send(socket: &mut Socket, message: &AgentMessage) -> anyhow::Result<()> {
    socket
        .send(Message::Text(serde_json::to_string(message)?.into()))
        .await
        .context("sending")
}

/// What arrived from the server.
enum Incoming {
    Message(ServerMessage),
    /// A pong or a message this agent does not understand: the server is
    /// alive, but there is nothing to do.
    Alive,
    Closed,
}

async fn receive(socket: &mut Socket) -> anyhow::Result<Incoming> {
    let Some(frame) = socket.next().await else {
        return Ok(Incoming::Closed);
    };
    Ok(match frame.context("receiving")? {
        Message::Text(text) => match serde_json::from_str(&text) {
            Ok(message) => Incoming::Message(message),
            Err(e) => {
                tracing::debug!("ignoring server message {text:?}: {e}");
                Incoming::Alive
            }
        },
        Message::Close(_) => Incoming::Closed,
        Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {
            Incoming::Alive
        }
    })
}

/// `https://host/` → `wss://host/api/agent/ws`.
fn websocket_url(server_url: &str) -> anyhow::Result<String> {
    let rest = if let Some(rest) = server_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = server_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        anyhow::bail!("server URL {server_url:?} must start with https:// or http://");
    };
    Ok(format!(
        "{}{}",
        rest.trim_end_matches('/'),
        mnix_protocol::AGENT_WS_PATH
    ))
}

/// Exponential backoff with jitter, so a fleet does not reconnect in step
/// after a server restart.
struct Backoff {
    next: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { next: MIN_BACKOFF }
    }
}

impl Backoff {
    fn next(&mut self) -> Duration {
        let base = self.next;
        self.next = (self.next * 2).min(MAX_BACKOFF);
        let mut byte = [0u8; 1];
        let jitter = if getrandom::fill(&mut byte).is_ok() {
            f64::from(byte[0]) / 255.0 * 0.25
        } else {
            0.0
        };
        base.mul_f64(1.0 + jitter)
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("installing a SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("stopping");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_urls_follow_the_server_scheme() {
        assert_eq!(
            websocket_url("https://mnix.example/").unwrap(),
            "wss://mnix.example/api/agent/ws"
        );
        assert_eq!(
            websocket_url("http://10.0.0.1:8080").unwrap(),
            "ws://10.0.0.1:8080/api/agent/ws"
        );
        assert!(websocket_url("mnix.example").is_err());
    }

    #[test]
    fn logs_are_sent_in_whole_lines_with_exact_offsets() {
        assert!(chunk_log(b"", 0, true).is_empty());
        assert_eq!(chunk_log(b"one\ntwo", 5, false), [(5, "one\n".to_owned())]);
        assert_eq!(
            chunk_log(b"one\ntwo", 5, true),
            [(5, "one\n".to_owned()), (9, "two".to_owned())]
        );
        let long = vec![b'x'; LOG_CHUNK + 10];
        let chunks = chunk_log(&long, 0, false);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1.len(), LOG_CHUNK);

        let text = sanitize(b"ok \xff\xfe \xc3\xa9 \0!");
        assert_eq!(text, "ok ?? \u{e9}  !");
        assert_eq!(text.len(), b"ok \xff\xfe \xc3\xa9 \0!".len());
    }

    #[test]
    fn backoff_doubles_up_to_the_maximum() {
        let mut backoff = Backoff::default();
        let delays: Vec<_> = (0..10).map(|_| backoff.next()).collect();
        assert!(delays[0] >= MIN_BACKOFF && delays[0] <= MIN_BACKOFF.mul_f64(1.25));
        assert!(delays[1] >= MIN_BACKOFF * 2);
        assert!(delays[9] >= MAX_BACKOFF && delays[9] <= MAX_BACKOFF.mul_f64(1.25));
    }
}
