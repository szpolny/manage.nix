//! End-to-end tests of the HTTP API and agent WebSocket against a real
//! listener and a SQLite store. (The VM tests repeat this with real agents
//! and both backends.)

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mnix_protocol::api::{
    CreatedMachine, Deployment, DeploymentStatus, EnrollResponse, EnrollmentToken, LogEntry,
    Machine, MachineDeploymentStatus, MachineStatus,
};
use mnix_protocol::{
    Activate, ActivationMode, ActivationPhase, ActivationReport, ActivationStatus,
    ActivationTarget, AgentMessage, Facts, Hello, ServerMessage, PROTOCOL_VERSION,
};
use serde_json::json;
use time::OffsetDateTime;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::app::{self, AppState, Settings};
use crate::secret;

const ADMIN_TOKEN: &str = "test-admin-token-0123456789";

struct Server {
    base: String,
    state: Arc<AppState>,
    _dir: tempfile::TempDir,
    shutdown: tokio::sync::watch::Sender<bool>,
}

async fn start(heartbeat_timeout: Duration) -> Server {
    start_with(heartbeat_timeout, Duration::from_secs(600)).await
}

async fn start_with(heartbeat_timeout: Duration, activation_timeout: Duration) -> Server {
    launch(heartbeat_timeout, activation_timeout, None).await
}

async fn launch(
    heartbeat_timeout: Duration,
    activation_timeout: Duration,
    web_dir: Option<std::path::PathBuf>,
) -> Server {
    launch_building_on(
        heartbeat_timeout,
        activation_timeout,
        web_dir,
        crate::build::BuildOn::Server,
    )
    .await
}

async fn launch_building_on(
    heartbeat_timeout: Duration,
    activation_timeout: Duration,
    web_dir: Option<std::path::PathBuf>,
    build_on: crate::build::BuildOn,
) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("db.sqlite").display());
    let store = mnix_store::connect(&url).await.unwrap();
    store.migrate().await.unwrap();
    let organization = store
        .ensure_organization("default", OffsetDateTime::now_utc())
        .await
        .unwrap();
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = Arc::new(AppState {
        store,
        organization,
        admin_token_hash: secret::hash(ADMIN_TOKEN),
        settings: Settings {
            heartbeat_interval: Duration::from_secs(1),
            heartbeat_timeout,
            default_token_ttl: Duration::from_secs(3600),
            confirm_timeout: Duration::from_secs(60),
            activation_timeout,
            build_timeout: Duration::from_secs(60),
            session_lifetime: Duration::from_secs(3600),
            secure_cookies: true,
        },
        shutdown: shutdown_rx,
        builder: crate::build::Builder {
            state_dir: dir.path().to_owned(),
            signing_key: mnix_nix::SecretKey::generate("mnix-test-1").unwrap(),
            attic: None,
            build_on,
            ssh_keys: Default::default(),
            git_lock: tokio::sync::Mutex::new(()),
        },
        queue: tokio::sync::Notify::new(),
        activations: tokio::sync::watch::channel(0).0,
        web_dir,
    });
    tokio::spawn(crate::agent::reap_stale_sessions(state.clone()));
    tokio::spawn(crate::deploy::run_worker(state.clone()));
    tokio::spawn(crate::deploy::run_sweeper(state.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    let router = app::router(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    Server {
        base,
        state,
        _dir: dir,
        shutdown,
    }
}

/// A response to [`Server::raw`].
struct Raw {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Raw {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(json!(null))
    }

    /// The `name=value` part of the Set-Cookie header.
    fn cookie(&self) -> String {
        let set = self.header("set-cookie").expect("a Set-Cookie header");
        set.split(';').next().unwrap().to_owned()
    }
}

impl Server {
    /// Sends a request on a blocking thread (ureq is synchronous) and
    /// returns the status and JSON body.
    async fn request(
        &self,
        method: &'static str,
        path: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        let url = format!("http://{}{path}", self.base);
        let token = token.map(str::to_owned);
        tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build()
                .into();
            let auth = token.map(|t| format!("Bearer {t}"));
            let mut response = match method {
                "GET" => {
                    let mut req = agent.get(&url);
                    if let Some(auth) = &auth {
                        req = req.header("Authorization", auth);
                    }
                    req.call()
                }
                "PUT" => {
                    let mut req = agent.put(&url);
                    if let Some(auth) = &auth {
                        req = req.header("Authorization", auth);
                    }
                    req.send_json(body.unwrap_or(json!({})))
                }
                _ => {
                    let mut req = agent.post(&url);
                    if let Some(auth) = &auth {
                        req = req.header("Authorization", auth);
                    }
                    req.send_json(body.unwrap_or(json!({})))
                }
            }
            .unwrap();
            let status = response.status().as_u16();
            let body = response.body_mut().read_json().unwrap_or(json!(null));
            (status, body)
        })
        .await
        .unwrap()
    }

    /// Sends a request with arbitrary headers and returns everything.
    async fn raw(
        &self,
        method: &'static str,
        path: &str,
        headers: &[(&'static str, &str)],
        body: Option<serde_json::Value>,
    ) -> Raw {
        let url = format!("http://{}{path}", self.base);
        let headers: Vec<(&'static str, String)> =
            headers.iter().map(|(n, v)| (*n, (*v).to_owned())).collect();
        tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build()
                .into();
            let mut response = match method {
                "GET" | "DELETE" => {
                    let mut req = if method == "GET" {
                        agent.get(&url)
                    } else {
                        agent.delete(&url)
                    };
                    for (name, value) in &headers {
                        req = req.header(*name, value);
                    }
                    req.call()
                }
                _ => {
                    let mut req = if method == "PUT" {
                        agent.put(&url)
                    } else {
                        agent.post(&url)
                    };
                    for (name, value) in &headers {
                        req = req.header(*name, value);
                    }
                    req.send_json(body.unwrap_or(json!({})))
                }
            }
            .unwrap();
            Raw {
                status: response.status().as_u16(),
                headers: response
                    .headers()
                    .iter()
                    .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or("").to_owned()))
                    .collect(),
                body: response.body_mut().read_to_string().unwrap_or_default(),
            }
        })
        .await
        .unwrap()
    }

    /// Creates a user and logs in; returns the session cookie.
    async fn login(&self, username: &str, password: &str) -> String {
        crate::auth::set_password(
            &*self.state.store,
            self.state.organization.id,
            username,
            password,
        )
        .await
        .unwrap();
        let response = self
            .raw(
                "POST",
                "/api/session",
                &[],
                Some(json!({"username": username, "password": password})),
            )
            .await;
        assert_eq!(response.status, 200, "{}", response.body);
        response.cookie()
    }

    async fn admin(
        &self,
        method: &'static str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        self.request(method, path, Some(ADMIN_TOKEN), body).await
    }

    async fn add_machine(&self, name: &str) -> CreatedMachine {
        let (status, body) = self
            .admin("POST", "/api/machines", Some(json!({ "name": name })))
            .await;
        assert_eq!(status, 201, "{body}");
        serde_json::from_value(body).unwrap()
    }

    async fn enroll(&self, token: &str) -> (u16, serde_json::Value) {
        self.request(
            "POST",
            mnix_protocol::ENROLL_PATH,
            None,
            Some(json!({ "token": token })),
        )
        .await
    }

    async fn machine(&self, name: &str) -> Machine {
        let (status, body) = self
            .admin("GET", &format!("/api/machines/{name}"), None)
            .await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_value(body).unwrap()
    }

    async fn deployment(&self, id: &str) -> Deployment {
        let (status, body) = self
            .admin("GET", &format!("/api/deployments/{id}"), None)
            .await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_value(body).unwrap()
    }

    async fn wait_for_deployment(&self, id: &str, status: DeploymentStatus) -> Deployment {
        for _ in 0..150 {
            let deployment = self.deployment(id).await;
            if deployment.status == status {
                return deployment;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!(
            "deployment {id} never became {status}: {:?}",
            self.deployment(id).await
        );
    }

    async fn logs(&self, id: &str) -> Vec<LogEntry> {
        let (status, body) = self
            .admin("GET", &format!("/api/deployments/{id}/logs"), None)
            .await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_value(body).unwrap()
    }

    /// Adds and enrolls a machine; returns its credential.
    async fn enrolled(&self, name: &str) -> String {
        let created = self.add_machine(name).await;
        let (status, body) = self.enroll(&created.enrollment_token.token).await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_value::<EnrollResponse>(body)
            .unwrap()
            .credential
    }

    /// Connects as the machine and completes the handshake.
    async fn session(&self, credential: &str, hostname: &str) -> Socket {
        let mut ws = self.connect(credential).await.unwrap();
        ws.send(text(&AgentMessage::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            agent_version: "0.1.0".to_owned(),
            facts: facts(hostname),
        })))
        .await
        .unwrap();
        assert!(matches!(
            receive(&mut ws).await,
            Some(ServerMessage::Welcome { .. })
        ));
        ws
    }

    async fn wait_for_status(&self, name: &str, status: MachineStatus) -> Machine {
        for _ in 0..100 {
            let machine = self.machine(name).await;
            if machine.status == status {
                return machine;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{name} never became {status}");
    }

    async fn connect(
        &self,
        credential: &str,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::Error,
    > {
        let mut request = format!("ws://{}{}", self.base, mnix_protocol::AGENT_WS_PATH)
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {credential}").parse().unwrap(),
        );
        tokio_tungstenite::connect_async(request)
            .await
            .map(|(ws, _)| ws)
    }
}

fn facts(hostname: &str) -> Facts {
    Facts {
        hostname: hostname.to_owned(),
        current_system: "/nix/store/00000000000000000000000000000000-nixos-system-a".to_owned(),
        booted_system: None,
        os: mnix_protocol::Os::Nixos,
        os_version: Some("26.11".to_owned()),
        configuration_revision: None,
        kernel: "6.12.0".to_owned(),
        reboot_required: false,
        generations: Vec::new(),
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn text(message: &AgentMessage) -> Message {
    Message::Text(serde_json::to_string(message).unwrap().into())
}

async fn receive(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Option<ServerMessage> {
    loop {
        match ws.next().await? {
            Ok(Message::Text(t)) => return Some(serde_json::from_str(&t).unwrap()),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

#[tokio::test]
async fn the_admin_api_requires_the_admin_token() {
    let server = start(Duration::from_secs(30)).await;
    assert_eq!(
        server.request("GET", "/api/machines", None, None).await.0,
        401
    );
    assert_eq!(
        server
            .request("GET", "/api/machines", Some("wrong-token-0123456789"), None)
            .await
            .0,
        401
    );
    let (status, body) = server.admin("GET", "/api/machines", None).await;
    assert_eq!((status, body), (200, json!([])));
}

#[tokio::test]
async fn machines_are_validated_and_unique() {
    let server = start(Duration::from_secs(30)).await;
    let created = server.add_machine("web-1").await;
    assert_eq!(created.machine.status, MachineStatus::Pending);
    assert!(created
        .enrollment_token
        .token
        .starts_with(secret::ENROLLMENT_TOKEN_PREFIX));

    let (status, body) = server
        .admin("POST", "/api/machines", Some(json!({ "name": "web-1" })))
        .await;
    assert_eq!(status, 409, "{body}");
    let (status, _) = server
        .admin("POST", "/api/machines", Some(json!({ "name": "bad name" })))
        .await;
    assert_eq!(status, 400);
    let (status, _) = server
        .admin(
            "POST",
            "/api/machines",
            Some(json!({ "name": "x", "enrollment_token_ttl_secs": 0 })),
        )
        .await;
    assert_eq!(status, 400);

    assert_eq!(server.machine(&created.machine.id).await.name, "web-1");
    assert_eq!(server.admin("GET", "/api/machines/nope", None).await.0, 404);
}

#[tokio::test]
async fn enrollment_tokens_are_one_time_and_expire() {
    let server = start(Duration::from_secs(30)).await;
    let created = server.add_machine("web-1").await;
    let token = created.enrollment_token.token;

    let (status, body) = server.enroll(&token).await;
    assert_eq!(status, 200, "{body}");
    let enrolled: EnrollResponse = serde_json::from_value(body).unwrap();
    assert_eq!(enrolled.machine_id, created.machine.id);
    assert!(enrolled.credential.starts_with(secret::CREDENTIAL_PREFIX));
    assert_eq!(server.machine("web-1").await.status, MachineStatus::Offline);

    let (status, body) = server.enroll(&token).await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(server.enroll("mnix_enroll_nonsense").await.0, 401);

    let (status, body) = server
        .admin(
            "POST",
            "/api/machines",
            Some(json!({ "name": "short-lived", "enrollment_token_ttl_secs": 1 })),
        )
        .await;
    assert_eq!(status, 201);
    let expiring: CreatedMachine = serde_json::from_value(body).unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(server.enroll(&expiring.enrollment_token.token).await.0, 401);

    // A fresh token works for the same machine.
    let (status, body) = server
        .admin(
            "POST",
            "/api/machines/short-lived/enrollment-tokens",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, 201);
    let fresh: EnrollmentToken = serde_json::from_value(body).unwrap();
    assert_eq!(server.enroll(&fresh.token).await.0, 200);
}

#[tokio::test]
async fn sessions_report_facts_and_go_offline() {
    let server = start(Duration::from_secs(2)).await;
    let created = server.add_machine("web-1").await;
    let (_, body) = server.enroll(&created.enrollment_token.token).await;
    let enrolled: EnrollResponse = serde_json::from_value(body).unwrap();

    assert!(server.connect("mnix_cred_wrong").await.is_err());

    let mut ws = server.connect(&enrolled.credential).await.unwrap();
    ws.send(text(&AgentMessage::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        agent_version: "0.1.0".to_owned(),
        facts: facts("web-1"),
    })))
    .await
    .unwrap();
    let Some(ServerMessage::Welcome {
        machine_id,
        heartbeat_interval_secs,
        heartbeat_timeout_secs,
        ..
    }) = receive(&mut ws).await
    else {
        panic!("expected welcome");
    };
    assert_eq!(machine_id, created.machine.id);
    assert_eq!((heartbeat_interval_secs, heartbeat_timeout_secs), (1, 2));

    let machine = server.machine("web-1").await;
    assert_eq!(machine.status, MachineStatus::Online);
    assert_eq!(machine.facts, Some(facts("web-1")));
    assert_eq!(machine.current_system, Some(facts("web-1").current_system));
    assert_eq!(machine.agent_version.as_deref(), Some("0.1.0"));

    // Unknown messages are ignored; heartbeats keep the machine online
    // past the timeout.
    ws.send(Message::Text(r#"{"type": "from_the_future"}"#.into()))
        .await
        .unwrap();
    let mut updated = facts("web-1");
    updated.reboot_required = true;
    ws.send(text(&AgentMessage::Facts {
        facts: updated.clone(),
    }))
    .await
    .unwrap();
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        ws.send(text(&AgentMessage::Heartbeat)).await.unwrap();
    }
    let machine = server.machine("web-1").await;
    assert_eq!(machine.status, MachineStatus::Online);
    assert_eq!(machine.facts, Some(updated));

    // Silence: the server times the session out and closes the socket.
    let closed = tokio::time::timeout(Duration::from_secs(5), receive(&mut ws)).await;
    assert!(matches!(closed, Ok(None)), "{closed:?}");
    server
        .wait_for_status("web-1", MachineStatus::Offline)
        .await;
}

#[tokio::test]
async fn protocol_errors_close_the_session() {
    let server = start(Duration::from_secs(30)).await;
    let created = server.add_machine("web-1").await;
    let (_, body) = server.enroll(&created.enrollment_token.token).await;
    let enrolled: EnrollResponse = serde_json::from_value(body).unwrap();

    let mut ws = server.connect(&enrolled.credential).await.unwrap();
    ws.send(text(&AgentMessage::Heartbeat)).await.unwrap();
    assert!(matches!(
        receive(&mut ws).await,
        Some(ServerMessage::Error { message }) if message.contains("expected hello")
    ));

    let mut ws = server.connect(&enrolled.credential).await.unwrap();
    ws.send(Message::Text(
        json!({
            "type": "hello",
            "protocol_version": 99,
            "agent_version": "9.0.0",
            "facts": facts("web-1"),
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    let reply = receive(&mut ws).await;
    assert!(
        matches!(&reply, Some(ServerMessage::Error { message }) if message.contains("protocol version v99")),
        "{reply:?}"
    );
    assert_eq!(server.machine("web-1").await.status, MachineStatus::Offline);
}

#[tokio::test]
async fn a_reconnecting_agent_replaces_its_session_and_shutdown_closes_it() {
    let server = start(Duration::from_secs(30)).await;
    let created = server.add_machine("web-1").await;
    let (_, body) = server.enroll(&created.enrollment_token.token).await;
    let enrolled: EnrollResponse = serde_json::from_value(body).unwrap();
    let hello = text(&AgentMessage::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        agent_version: "0.1.0".to_owned(),
        facts: facts("web-1"),
    }));

    let mut old = server.connect(&enrolled.credential).await.unwrap();
    old.send(hello.clone()).await.unwrap();
    assert!(receive(&mut old).await.is_some());
    let mut new = server.connect(&enrolled.credential).await.unwrap();
    new.send(hello).await.unwrap();
    assert!(receive(&mut new).await.is_some());

    // The old connection's next heartbeat finds its session replaced.
    old.send(text(&AgentMessage::Heartbeat)).await.unwrap();
    assert!(receive(&mut old).await.is_none());
    assert_eq!(server.machine("web-1").await.status, MachineStatus::Online);

    server.shutdown.send(true).unwrap();
    assert!(receive(&mut new).await.is_none());
    server
        .wait_for_status("web-1", MachineStatus::Offline)
        .await;
}

/// The next message within `wait`, if any.
async fn receive_within(ws: &mut Socket, wait: Duration) -> Option<ServerMessage> {
    tokio::time::timeout(wait, receive(ws)).await.ok().flatten()
}

fn succeeded(store_path: &str) -> ActivationReport {
    ActivationReport {
        store_path: store_path.to_owned(),
        mode: ActivationMode::Switch,
        status: ActivationStatus::Succeeded,
        system_changed: true,
        previous_system: Some(facts("web-1").current_system),
        message: None,
        failed_checks: vec![],
    }
}

#[tokio::test]
async fn a_rollback_reaches_the_agent_and_is_confirmed() {
    let server = start(Duration::from_secs(30)).await;
    let credential = server.enrolled("web-1").await;
    let mut ws = server.session(&credential, "web-1").await;

    let (status, body) = server
        .admin(
            "POST",
            "/api/machines/web-1/rollback",
            Some(json!({"requested_by": "alice"})),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let deployment: Deployment = serde_json::from_value(body).unwrap();
    assert_eq!(deployment.status, DeploymentStatus::Activating);
    assert_eq!(deployment.requested_by, "alice");

    let Some(ServerMessage::Activate(Activate {
        deployment_id,
        target,
        mode,
        confirm_timeout_secs,
        ..
    })) = receive_within(&mut ws, Duration::from_secs(5)).await
    else {
        panic!("expected activate");
    };
    assert_eq!(deployment_id, deployment.id);
    assert_eq!(target, ActivationTarget::PreviousGeneration);
    assert_eq!((mode, confirm_timeout_secs), (ActivationMode::Switch, 60));

    // A new session gets the request again until the agent acknowledges it.
    let mut ws = server.session(&credential, "web-1").await;
    assert!(matches!(
        receive_within(&mut ws, Duration::from_secs(5)).await,
        Some(ServerMessage::Activate(_))
    ));
    let status = |phase| {
        text(&AgentMessage::ActivationStatus {
            deployment_id: deployment.id.clone(),
            phase,
        })
    };
    ws.send(status(ActivationPhase::Accepted)).await.unwrap();
    let mut ws = server.session(&credential, "web-1").await;
    ws.send(text(&AgentMessage::Heartbeat)).await.unwrap();
    assert!(receive_within(&mut ws, Duration::from_millis(500))
        .await
        .is_none());
    let machine = &server.deployment(&deployment.id).await.machines[0];
    assert_eq!(machine.phase, Some(ActivationPhase::Accepted));

    ws.send(status(ActivationPhase::AwaitingConfirmation))
        .await
        .unwrap();
    assert!(matches!(
        receive_within(&mut ws, Duration::from_secs(5)).await,
        Some(ServerMessage::Confirm { deployment_id }) if deployment_id == deployment.id
    ));

    for (offset, chunk) in [(0, "one\n"), (0, "one\ntwo\n")] {
        ws.send(text(&AgentMessage::ActivationLog {
            deployment_id: deployment.id.clone(),
            offset,
            text: chunk.to_owned(),
        }))
        .await
        .unwrap();
    }
    let previous = "/nix/store/11111111111111111111111111111111-nixos-system-old";
    ws.send(text(&AgentMessage::ActivationResult {
        deployment_id: deployment.id.clone(),
        report: succeeded(previous),
    }))
    .await
    .unwrap();
    assert!(matches!(
        receive_within(&mut ws, Duration::from_secs(5)).await,
        Some(ServerMessage::ResultReceived { deployment_id }) if deployment_id == deployment.id
    ));

    let done = server
        .wait_for_deployment(&deployment.id, DeploymentStatus::Succeeded)
        .await;
    assert_eq!(done.machines[0].status, MachineDeploymentStatus::Succeeded);
    assert_eq!(done.machines[0].store_path.as_deref(), Some(previous));
    assert!(done.finished_at.is_some());
    let logs: Vec<_> = server
        .logs(&deployment.id)
        .await
        .into_iter()
        .map(|l| (l.machine, l.source, l.text))
        .collect();
    assert_eq!(
        logs,
        [
            (Some("web-1".into()), "agent".into(), "one\n".into()),
            (Some("web-1".into()), "agent".into(), "two\n".into()),
        ]
    );

    // A late repeat changes nothing but is still acknowledged.
    ws.send(text(&AgentMessage::ActivationResult {
        deployment_id: deployment.id.clone(),
        report: ActivationReport {
            status: ActivationStatus::Failed,
            ..succeeded(previous)
        },
    }))
    .await
    .unwrap();
    assert!(matches!(
        receive_within(&mut ws, Duration::from_secs(5)).await,
        Some(ServerMessage::ResultReceived { .. })
    ));
    assert_eq!(
        server.deployment(&deployment.id).await.status,
        DeploymentStatus::Succeeded
    );
    let (status, body) = server.admin("GET", "/api/deployments", None).await;
    assert_eq!(status, 200);
    assert_eq!(body.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn finished_activations_are_not_confirmed() {
    let server = start(Duration::from_secs(30)).await;
    let credential = server.enrolled("web-1").await;
    let mut ws = server.session(&credential, "web-1").await;
    let (_, body) = server
        .admin("POST", "/api/machines/web-1/rollback", Some(json!({})))
        .await;
    let deployment: Deployment = serde_json::from_value(body).unwrap();
    assert!(receive_within(&mut ws, Duration::from_secs(5))
        .await
        .is_some());
    ws.send(text(&AgentMessage::ActivationResult {
        deployment_id: deployment.id.clone(),
        report: ActivationReport {
            status: ActivationStatus::RolledBack,
            system_changed: false,
            message: Some("health checks did not pass".to_owned()),
            ..succeeded("")
        },
    }))
    .await
    .unwrap();
    assert!(receive_within(&mut ws, Duration::from_secs(5))
        .await
        .is_some());
    let done = server
        .wait_for_deployment(&deployment.id, DeploymentStatus::Failed)
        .await;
    assert_eq!(done.machines[0].status, MachineDeploymentStatus::RolledBack);
    assert_eq!(
        done.machines[0].message.as_deref(),
        Some("health checks did not pass")
    );
    ws.send(text(&AgentMessage::ActivationStatus {
        deployment_id: deployment.id.clone(),
        phase: ActivationPhase::AwaitingConfirmation,
    }))
    .await
    .unwrap();
    assert!(receive_within(&mut ws, Duration::from_millis(500))
        .await
        .is_none());
}

#[tokio::test]
async fn deployments_are_validated_and_failed_builds_reported() {
    let server = start(Duration::from_secs(30)).await;
    server.enrolled("web-1").await;
    server.add_machine("pending").await;
    let deploy = |targets: serde_json::Value| {
        server.admin(
            "POST",
            "/api/deployments",
            Some(json!({ "targets": targets })),
        )
    };
    assert_eq!(deploy(json!([])).await.0, 400);
    assert_eq!(deploy(json!(["nope"])).await.0, 400);
    assert_eq!(deploy(json!(["tag:web"])).await.0, 400);
    let (status, body) = deploy(json!(["web-1"])).await;
    assert_eq!(status, 400);
    assert!(
        body["error"].as_str().unwrap().contains("no configuration"),
        "{body}"
    );
    assert_eq!(deploy(json!(["pending"])).await.0, 400);

    let repo = |name: &str, url: &str| {
        server.admin(
            "POST",
            "/api/repositories",
            Some(json!({ "name": name, "url": url })),
        )
    };
    assert_eq!(repo("infra", "ext::sh -c id").await.0, 400);
    assert_eq!(repo("bad name", "/srv/git/infra").await.0, 400);
    let missing = server._dir.path().join("no-such-repository");
    let (status, body) = repo("infra", missing.to_str().unwrap()).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(repo("infra", "/srv/x").await.0, 409);
    let (status, body) = server.admin("GET", "/api/repositories", None).await;
    assert_eq!((status, body[0]["name"].clone()), (200, json!("infra")));

    let configure = |body: serde_json::Value| {
        server.admin("PUT", "/api/machines/web-1/configuration", Some(body))
    };
    assert_eq!(configure(json!({"repository": "nope"})).await.0, 400);
    assert_eq!(
        configure(json!({"repository": "infra", "configuration": "a b"}))
            .await
            .0,
        400
    );
    let (status, body) = configure(json!({"repository": "infra"})).await;
    assert_eq!(status, 200, "{body}");
    let machine: Machine = serde_json::from_value(body).unwrap();
    assert_eq!(machine.repository.as_deref(), Some("infra"));
    assert_eq!(machine.configuration.as_deref(), Some("web-1"));
    let (status, body) = server
        .admin(
            "PUT",
            "/api/machines/web-1/tags",
            Some(json!({"tags": ["web", "eu"]})),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["tags"], json!(["eu", "web"]));
    assert_eq!(
        server
            .admin(
                "PUT",
                "/api/machines/web-1/tags",
                Some(json!({"tags": ["no way"]}))
            )
            .await
            .0,
        400
    );

    let (status, body) = server
        .admin(
            "POST",
            "/api/deployments",
            Some(json!({"targets": ["tag:web"], "rev": "--upload-pack=x"})),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = deploy(json!(["tag:web"])).await;
    assert_eq!(status, 201, "{body}");
    let deployment: Deployment = serde_json::from_value(body).unwrap();
    assert_eq!(deployment.rev.as_deref(), Some("HEAD"));
    assert_eq!(deployment.repository.as_deref(), Some("infra"));
    assert_eq!(deployment.machines[0].machine, "web-1");

    // The repository does not exist, so fetching it fails.
    let failed = server
        .wait_for_deployment(&deployment.id, DeploymentStatus::Failed)
        .await;
    assert!(failed.message.is_some(), "{failed:?}");
    assert_eq!(failed.machines[0].status, MachineDeploymentStatus::Failed);
    let logs = server.logs(&deployment.id).await;
    assert!(logs.iter().any(|l| l.text.contains("cloning")), "{logs:?}");

    let (status, body) = server.admin("GET", "/api/signing-key", None).await;
    assert_eq!(status, 200);
    assert!(body["public_key"]
        .as_str()
        .unwrap()
        .starts_with("mnix-test-1:"));
}

#[tokio::test]
async fn unanswered_activations_time_out() {
    let server = start_with(Duration::from_secs(30), Duration::from_secs(1)).await;
    server.enrolled("web-1").await;
    let (status, body) = server
        .admin("POST", "/api/machines/web-1/rollback", Some(json!({})))
        .await;
    assert_eq!(status, 201, "{body}");
    let deployment: Deployment = serde_json::from_value(body).unwrap();
    let failed = server
        .wait_for_deployment(&deployment.id, DeploymentStatus::Failed)
        .await;
    assert!(
        failed.machines[0]
            .message
            .as_deref()
            .unwrap()
            .contains("did not finish activating"),
        "{failed:?}"
    );
}

#[tokio::test]
async fn users_log_in_and_act_as_themselves() {
    let server = start(Duration::from_secs(30)).await;
    let state = &server.state;
    crate::auth::set_password(
        &*state.store,
        state.organization.id,
        "alice",
        "correct horse",
    )
    .await
    .unwrap();
    let login = |username: &'static str, password: &'static str| {
        server.raw(
            "POST",
            "/api/session",
            &[],
            Some(json!({"username": username, "password": password})),
        )
    };
    for (username, password) in [("alice", "wrong horse"), ("bob", "correct horse")] {
        let response = login(username, password).await;
        assert_eq!(response.status, 401, "{}", response.body);
        assert!(response.header("set-cookie").is_none());
    }
    let response = login("alice", "correct horse").await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.json()["username"], "alice");
    let set_cookie = response.header("set-cookie").unwrap();
    for attribute in ["HttpOnly", "SameSite=Strict", "Secure", "Path=/"] {
        assert!(set_cookie.contains(attribute), "{set_cookie}");
    }
    let cookie = response.cookie();
    assert!(cookie.starts_with("mnix_session=mnix_session_"), "{cookie}");

    let response = server
        .raw("GET", "/api/session", &[("cookie", &cookie)], None)
        .await;
    assert_eq!(response.json()["username"], "alice");
    let response = server
        .raw("GET", "/api/machines", &[("cookie", &cookie)], None)
        .await;
    assert_eq!(response.status, 200);

    // Changes need the CSRF header.
    let add = json!({"name": "web-1"});
    let response = server
        .raw(
            "POST",
            "/api/machines",
            &[("cookie", &cookie)],
            Some(add.clone()),
        )
        .await;
    assert_eq!(response.status, 403, "{}", response.body);
    let with_csrf = [("cookie", cookie.as_str()), ("x-mnix-csrf", "1")];
    let response = server
        .raw("POST", "/api/machines", &with_csrf, Some(add))
        .await;
    assert_eq!(response.status, 201, "{}", response.body);
    let created: CreatedMachine = serde_json::from_str(&response.body).unwrap();
    server.enroll(&created.enrollment_token.token).await;

    // Deployments record who asked, whatever the request says.
    let response = server
        .raw(
            "POST",
            "/api/machines/web-1/rollback",
            &with_csrf,
            Some(json!({"requested_by": "mallory"})),
        )
        .await;
    assert_eq!(response.status, 201, "{}", response.body);
    assert_eq!(response.json()["requested_by"], "alice");

    // Changing the password needs the current one and ends other sessions.
    let other = login("alice", "correct horse").await.cookie();
    let change = |current: &'static str| {
        server.raw(
            "PUT",
            "/api/session/password",
            &with_csrf,
            Some(json!({"current_password": current, "new_password": "battery staple"})),
        )
    };
    assert_eq!(change("wrong horse").await.status, 403);
    assert_eq!(change("correct horse").await.status, 204);
    assert_eq!(login("alice", "correct horse").await.status, 401);
    assert_eq!(login("alice", "battery staple").await.status, 200);
    let response = server
        .raw("GET", "/api/session", &[("cookie", &other)], None)
        .await;
    assert_eq!(response.status, 401);

    // Logging out ends the session.
    let response = server.raw("DELETE", "/api/session", &with_csrf, None).await;
    assert_eq!(response.status, 204);
    assert!(response.header("set-cookie").unwrap().contains("Max-Age=0"));
    let response = server
        .raw("GET", "/api/session", &[("cookie", &cookie)], None)
        .await;
    assert_eq!(response.status, 401);

    // The admin token still works, and a wrong one is not a fallback to
    // the cookie.
    let (status, _) = server.admin("GET", "/api/session", None).await;
    assert_eq!(status, 200);
    let fresh = server.login("alice", "battery staple").await;
    let response = server
        .raw(
            "GET",
            "/api/machines",
            &[("cookie", &fresh), ("authorization", "Bearer wrong")],
            None,
        )
        .await;
    assert_eq!(response.status, 401);
}

#[tokio::test]
async fn the_first_user_is_created_once() {
    let server = start(Duration::from_secs(30)).await;
    let state = &server.state;
    let dir = tempfile::tempdir().unwrap();
    let generated = dir.path().join("initial-admin-password");
    crate::auth::ensure_initial_user(&*state.store, state.organization.id, None, &generated)
        .await
        .unwrap();
    let password = std::fs::read_to_string(&generated).unwrap();
    let password = password.trim();
    std::fs::remove_file(&generated).unwrap();
    crate::auth::ensure_initial_user(&*state.store, state.organization.id, None, &generated)
        .await
        .unwrap();
    assert!(!generated.exists());
    let response = server
        .raw(
            "POST",
            "/api/session",
            &[],
            Some(json!({"username": "admin", "password": password})),
        )
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
}

#[tokio::test]
async fn machines_show_revisions_history_and_generations() {
    let server = start(Duration::from_secs(30)).await;
    let credential = server.enrolled("web-1").await;
    let mut ws = server.session(&credential, "web-1").await;
    let old = "/nix/store/11111111111111111111111111111111-nixos-system-old";
    let new = "/nix/store/22222222222222222222222222222222-nixos-system-new";
    let generation = |number, store_path: &str, current| mnix_protocol::Generation {
        number,
        store_path: store_path.to_owned(),
        created_at: "2026-10-22T08:00:00Z".to_owned(),
        current,
    };
    ws.send(text(&AgentMessage::Facts {
        facts: Facts {
            current_system: new.to_owned(),
            booted_system: Some(old.to_owned()),
            generations: vec![generation(1, old, false), generation(2, new, true)],
            ..facts("web-1")
        },
    }))
    .await
    .unwrap();

    // A deployment built `new` from commit c0ffee.
    let state = &server.state;
    let machine = server.machine("web-1").await;
    let machine_id = machine.id.parse().unwrap();
    let repository = state
        .store
        .create_repository(
            state.organization.id,
            "infra",
            "/srv/git/infra",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let deployment_id = uuid::Uuid::new_v4();
    state
        .store
        .create_deployment(&mnix_store::NewDeployment {
            id: deployment_id,
            organization_id: state.organization.id,
            kind: mnix_protocol::api::DeploymentKind::Deploy,
            status: DeploymentStatus::Building,
            repository_id: Some(repository.id),
            rev: Some("main".to_owned()),
            mode: ActivationMode::Switch,
            requested_by: "alice".to_owned(),
            created_at: OffsetDateTime::now_utc(),
            machines: vec![mnix_store::NewDeploymentMachine {
                machine_id,
                configuration: Some("web-1".to_owned()),
                status: MachineDeploymentStatus::Building,
            }],
        })
        .await
        .unwrap();
    state
        .store
        .set_deployment_commit(deployment_id, "c0ffee")
        .await
        .unwrap();
    state
        .store
        .release_activation(
            deployment_id,
            machine_id,
            &mnix_store::Release::System {
                store_path: new.to_owned(),
                signatures: Vec::new(),
            },
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

    let mut machine = server.machine("web-1").await;
    for _ in 0..50 {
        if machine.current_system.as_deref() == Some(new) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        machine = server.machine("web-1").await;
    }
    let revision = machine.revision.expect("the running system's revision");
    assert_eq!(revision.commit, "c0ffee");
    assert_eq!(revision.rev.as_deref(), Some("main"));
    assert_eq!(revision.repository.as_deref(), Some("infra"));
    let last = machine.last_deployment.expect("the last deployment");
    assert_eq!(last.deployment_id, deployment_id.to_string());
    assert_eq!(last.status, MachineDeploymentStatus::Activating);
    let (_, listed) = server.admin("GET", "/api/machines", None).await;
    assert_eq!(listed[0]["revision"]["commit"], "c0ffee");
    assert_eq!(
        listed[0]["last_deployment"]["deployment_id"],
        deployment_id.to_string()
    );

    let (status, history) = server
        .admin("GET", "/api/machines/web-1/deployments", None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(history.as_array().unwrap().len(), 1);
    assert_eq!(history[0]["requested_by"], "alice");

    let (status, generations) = server
        .admin("GET", "/api/machines/web-1/generations", None)
        .await;
    assert_eq!(status, 200);
    let generations: Vec<mnix_protocol::api::GenerationView> =
        serde_json::from_value(generations).unwrap();
    assert_eq!(
        generations
            .iter()
            .map(|g| (g.number, g.current, g.running, g.booted))
            .collect::<Vec<_>>(),
        [(2, true, true, false), (1, false, false, true)]
    );
    assert_eq!(generations[0].revision.as_ref().unwrap().commit, "c0ffee");
    assert!(generations[1].revision.is_none());

    // Neither system exists on this host, so there is nothing to compare.
    let (status, body) = server
        .admin("GET", "/api/machines/web-1/diff?from=1&to=2", None)
        .await;
    assert_eq!(status, 422, "{body}");
    let (status, _) = server
        .admin("GET", "/api/machines/web-1/diff?from=1&to=9", None)
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn deployment_events_stream_until_the_end() {
    let server = start(Duration::from_secs(30)).await;
    let credential = server.enrolled("web-1").await;
    let mut ws = server.session(&credential, "web-1").await;
    let (_, body) = server
        .admin("POST", "/api/machines/web-1/rollback", Some(json!({})))
        .await;
    let deployment: Deployment = serde_json::from_value(body).unwrap();
    let Some(ServerMessage::Activate(_)) = receive_within(&mut ws, Duration::from_secs(5)).await
    else {
        panic!("expected activate");
    };
    for (offset, chunk) in [(0, "one\n"), (4, "two\n")] {
        ws.send(text(&AgentMessage::ActivationLog {
            deployment_id: deployment.id.clone(),
            offset,
            text: chunk.to_owned(),
        }))
        .await
        .unwrap();
    }
    let previous = "/nix/store/11111111111111111111111111111111-nixos-system-old";
    ws.send(text(&AgentMessage::ActivationResult {
        deployment_id: deployment.id.clone(),
        report: succeeded(previous),
    }))
    .await
    .unwrap();
    server
        .wait_for_deployment(&deployment.id, DeploymentStatus::Succeeded)
        .await;

    // The stream ends by itself once the deployment has finished.
    let path = format!("/api/deployments/{}/events", deployment.id);
    let auth = format!("Bearer {ADMIN_TOKEN}");
    let response = server
        .raw("GET", &path, &[("authorization", &auth)], None)
        .await;
    assert_eq!(response.status, 200);
    assert!(response
        .header("content-type")
        .unwrap()
        .starts_with("text/event-stream"));
    let events: Vec<(String, Option<String>, String)> = response
        .body
        .split("\n\n")
        .filter(|block| !block.trim().is_empty() && !block.starts_with(':'))
        .map(|block| {
            let field = |name: &str| {
                block
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{name}:")))
                    .map(|v| v.trim_start().to_owned())
            };
            (
                field("event").unwrap(),
                field("id"),
                field("data").unwrap_or_default(),
            )
        })
        .collect();
    let kinds: Vec<_> = events.iter().map(|(kind, _, _)| kind.as_str()).collect();
    assert_eq!(kinds, ["deployment", "log", "log", "end"], "{events:?}");
    let first: Deployment = serde_json::from_str(&events[0].2).unwrap();
    assert_eq!(first.status, DeploymentStatus::Succeeded);
    let log: LogEntry = serde_json::from_str(&events[2].2).unwrap();
    assert_eq!(log.text, "two\n");
    assert_eq!(events[2].1, Some(log.id.to_string()));

    // Resuming after the first entry skips it.
    let first_id = events[1].1.clone().unwrap();
    let response = server
        .raw(
            "GET",
            &path,
            &[("authorization", &auth), ("last-event-id", &first_id)],
            None,
        )
        .await;
    assert_eq!(response.body.matches("event: log").count(), 1);
    assert!(response.body.contains("two"));
}

#[tokio::test]
async fn the_web_ui_is_served_with_a_fallback_to_index() {
    let web = tempfile::tempdir().unwrap();
    std::fs::write(web.path().join("index.html"), "<!doctype html>app").unwrap();
    std::fs::create_dir_all(web.path().join("_app/immutable")).unwrap();
    std::fs::write(web.path().join("_app/immutable/start.js"), "js").unwrap();
    let server = launch(
        Duration::from_secs(30),
        Duration::from_secs(600),
        Some(web.path().to_owned()),
    )
    .await;
    for path in ["/", "/machines/web-1", "/deploys"] {
        let response = server.raw("GET", path, &[], None).await;
        assert_eq!(response.status, 200, "{path}");
        assert_eq!(response.body, "<!doctype html>app", "{path}");
        assert_eq!(response.header("cache-control"), Some("no-cache"));
    }
    let response = server
        .raw("GET", "/_app/immutable/start.js", &[], None)
        .await;
    assert_eq!(response.body, "js");
    assert!(response
        .header("cache-control")
        .unwrap()
        .contains("immutable"));
    let response = server.raw("GET", "/api/nothing-here", &[], None).await;
    assert_eq!(response.status, 404);
    assert!(response.json()["error"].is_string());
    // The API still requires authentication.
    let response = server.raw("GET", "/api/machines", &[], None).await;
    assert_eq!(response.status, 401);
}

/// Runs git in `dir`, returning its trimmed output.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

impl Server {
    /// GETs `path` with a machine credential; returns the status and body.
    async fn download(&self, path: &str, credential: Option<&str>) -> (u16, Vec<u8>) {
        let url = format!("http://{}{path}", self.base);
        let auth = credential.map(|c| format!("Bearer {c}"));
        tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build()
                .into();
            let mut request = agent.get(&url);
            if let Some(auth) = &auth {
                request = request.header("Authorization", auth);
            }
            let mut response = request.call().unwrap();
            let status = response.status().as_u16();
            (status, response.body_mut().read_to_vec().unwrap())
        })
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn machines_build_from_source_the_server_sends() {
    let server = launch_building_on(
        Duration::from_secs(30),
        Duration::from_secs(600),
        None,
        crate::build::BuildOn::Machine,
    )
    .await;
    let work = server._dir.path().join("infra");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "--quiet"]);
    std::fs::write(work.join("flake.nix"), "{ outputs = _: { }; }\n").unwrap();
    git(&work, &["add", "flake.nix"]);
    git(&work, &["commit", "--quiet", "-m", "one"]);
    let first = git(&work, &["rev-parse", "HEAD"]);
    std::fs::write(work.join("README"), "two\n").unwrap();
    git(&work, &["add", "README"]);
    git(&work, &["commit", "--quiet", "-m", "two"]);
    let second = git(&work, &["rev-parse", "HEAD"]);

    let credential = server.enrolled("web-1").await;
    let other = server.enrolled("db-1").await;
    let (status, body) = server
        .admin(
            "POST",
            "/api/repositories",
            Some(json!({ "name": "infra", "url": work.to_str().unwrap() })),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let (status, body) = server
        .admin(
            "PUT",
            "/api/machines/web-1/configuration",
            Some(json!({ "repository": "infra" })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let mut ws = server.session(&credential, "web-1").await;
    let (status, body) = server
        .admin(
            "POST",
            "/api/deployments",
            Some(json!({ "targets": ["web-1"], "rev": second })),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let deployment_id = body["id"].as_str().unwrap().to_owned();

    let activate = loop {
        match receive(&mut ws).await {
            Some(ServerMessage::Activate(activate)) => break activate,
            Some(_) => {}
            None => panic!("the session closed"),
        }
    };
    assert_eq!(activate.deployment_id, deployment_id);
    assert!(activate.signatures.is_empty());
    let ActivationTarget::Build { commit, attribute } = &activate.target else {
        panic!("expected a build, got {:?}", activate.target);
    };
    assert_eq!(*commit, second);
    assert_eq!(
        attribute,
        "nixosConfigurations.\"web-1\".config.system.build.toplevel"
    );

    // Only the machine the deployment is building on may download it.
    let path = mnix_protocol::agent_source_path(&deployment_id);
    assert_eq!(server.download(&path, None).await.0, 401);
    assert_eq!(server.download(&path, Some(&other)).await.0, 404);
    let (status, full) = server.download(&path, Some(&credential)).await;
    assert_eq!(status, 200);
    let (status, partial) = server
        .download(
            &format!("{path}?have={first}&have=nonsense"),
            Some(&credential),
        )
        .await;
    assert_eq!(status, 200);
    assert!(partial.len() < full.len());

    // The agent's side: fetch the full bundle, then check that the partial
    // one applies on top of the first commit alone.
    let agent = server._dir.path().join("agent.git");
    std::fs::create_dir(&agent).unwrap();
    git(&agent, &["init", "--quiet", "--bare"]);
    let bundle = server._dir.path().join("source.bundle");
    std::fs::write(&bundle, &full).unwrap();
    let refspec = format!("{}:refs/heads/built", mnix_protocol::SOURCE_BUNDLE_REF);
    git(
        &agent,
        &["fetch", "--quiet", bundle.to_str().unwrap(), &refspec],
    );
    assert_eq!(git(&agent, &["rev-parse", "built"]), second);
    let older = server._dir.path().join("older.git");
    git(
        server._dir.path(),
        &[
            "clone",
            "--quiet",
            "--bare",
            work.to_str().unwrap(),
            "older.git",
        ],
    );
    git(&older, &["update-ref", "-d", "HEAD"]);
    for branch in git(&older, &["for-each-ref", "--format=%(refname)"]).lines() {
        git(&older, &["update-ref", "-d", branch]);
    }
    git(&older, &["update-ref", "refs/heads/old", &first]);
    std::fs::write(&bundle, &partial).unwrap();
    git(
        &older,
        &["fetch", "--quiet", bundle.to_str().unwrap(), &refspec],
    );
    assert_eq!(git(&older, &["rev-parse", "built"]), second);

    ws.send(text(&AgentMessage::ActivationStatus {
        deployment_id: deployment_id.clone(),
        phase: ActivationPhase::Building,
    }))
    .await
    .unwrap();
    let built = "/nix/store/11111111111111111111111111111111-nixos-system-web-1";
    ws.send(text(&AgentMessage::ActivationResult {
        deployment_id: deployment_id.clone(),
        report: succeeded(built),
    }))
    .await
    .unwrap();
    let deployment = server
        .wait_for_deployment(&deployment_id, DeploymentStatus::Succeeded)
        .await;
    assert_eq!(deployment.machines[0].store_path.as_deref(), Some(built));
    // Once it finished, the source is not served any more.
    assert_eq!(server.download(&path, Some(&credential)).await.0, 404);
}
