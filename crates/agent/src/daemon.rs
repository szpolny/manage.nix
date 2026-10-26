//! `mnix-agent run`: enroll if needed, then keep one outbound WebSocket to
//! the server open, reconnecting with backoff.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use mnix_protocol::{AgentMessage, Facts, Hello, ServerMessage, PROTOCOL_VERSION};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::credential::{self, Credential, EnrollError};
use crate::facts;

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How long to wait for the server's `welcome` after connecting.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(30);

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

    let mut backoff = Backoff::default();
    loop {
        let mut session = Session::new(&ws_url, &credential, &options.state_dir);
        tokio::select! {
            result = session.run() => {
                match result {
                    Ok(()) => tracing::info!("server closed the connection"),
                    Err(e) => tracing::warn!("connection failed: {e:#}"),
                }
                if session.welcomed {
                    backoff = Backoff::default();
                }
            }
            _ = &mut shutdown => {
                session.close().await;
                return Ok(());
            }
        }
        let delay = backoff.next();
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

struct Session<'a> {
    ws_url: &'a str,
    credential: &'a Credential,
    credential_path: PathBuf,
    socket: Option<Socket>,
    /// Whether the server accepted the `hello`.
    welcomed: bool,
}

impl<'a> Session<'a> {
    fn new(ws_url: &'a str, credential: &'a Credential, state_dir: &Path) -> Self {
        Self {
            ws_url,
            credential,
            credential_path: credential::path(state_dir),
            socket: None,
            welcomed: false,
        }
    }

    async fn run(&mut self) -> anyhow::Result<()> {
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
            None => anyhow::bail!("connection closed before welcome"),
        };
        self.welcomed = true;
        tracing::info!(
            "connected; heartbeat every {}",
            humantime::format_duration(interval)
        );

        let mut ticker = tokio::time::interval_at(Instant::now() + interval, interval);
        let mut last_heard = Instant::now();
        loop {
            tokio::select! {
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
                        Incoming::Message(ServerMessage::Welcome { .. }) | Incoming::Alive => {}
                        Incoming::Closed => return Ok(()),
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
    fn backoff_doubles_up_to_the_maximum() {
        let mut backoff = Backoff::default();
        let delays: Vec<_> = (0..10).map(|_| backoff.next()).collect();
        assert!(delays[0] >= MIN_BACKOFF && delays[0] <= MIN_BACKOFF.mul_f64(1.25));
        assert!(delays[1] >= MIN_BACKOFF * 2);
        assert!(delays[9] >= MAX_BACKOFF && delays[9] <= MAX_BACKOFF.mul_f64(1.25));
    }
}
