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
    _dir: tempfile::TempDir,
    shutdown: tokio::sync::watch::Sender<bool>,
}

async fn start(heartbeat_timeout: Duration) -> Server {
    start_with(heartbeat_timeout, Duration::from_secs(600)).await
}

async fn start_with(heartbeat_timeout: Duration, activation_timeout: Duration) -> Server {
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
        },
        shutdown: shutdown_rx,
        builder: crate::build::Builder {
            state_dir: dir.path().to_owned(),
            signing_key: mnix_nix::SecretKey::generate("mnix-test-1").unwrap(),
            attic: None,
        },
        queue: tokio::sync::Notify::new(),
        activations: tokio::sync::watch::channel(0).0,
    });
    tokio::spawn(crate::agent::reap_stale_sessions(state.clone()));
    tokio::spawn(crate::deploy::run_worker(state.clone()));
    tokio::spawn(crate::deploy::run_sweeper(state.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move { axum::serve(listener, app::router(state)).await });
    Server {
        base,
        _dir: dir,
        shutdown,
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
        nixos_version: Some("26.11".to_owned()),
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
