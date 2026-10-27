//! `mnix-server`: the manage.nix control plane.

mod agent;
mod api;
mod app;
mod build;
mod deploy;
#[cfg(test)]
mod e2e_tests;
mod secret;

use std::fs::File;
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use mnix_nix::SecretKey;
use mnix_store::Backend;
use time::OffsetDateTime;

/// The organization every machine belongs to until organizations arrive.
const DEFAULT_ORGANIZATION: &str = "default";

#[derive(Debug, Parser)]
#[command(name = "mnix-server", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Apply database migrations and exit.
    Migrate {
        #[command(flatten)]
        database: DatabaseArgs,
    },
    /// Run the control plane: the HTTP API and the agent endpoint.
    Serve(Box<ServeArgs>),
}

#[derive(Debug, Args)]
struct DatabaseArgs {
    /// `sqlite://...` or `postgres://...`
    #[arg(long, env = "MNIX_DATABASE_URL")]
    database_url: String,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[command(flatten)]
    database: DatabaseArgs,

    /// Address to listen on. Serve plain HTTP on a private address and put
    /// a TLS-terminating reverse proxy in front of it.
    #[arg(long, env = "MNIX_LISTEN", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,

    /// File containing the admin API token.
    #[arg(long, env = "MNIX_ADMIN_TOKEN_FILE")]
    admin_token_file: PathBuf,

    /// Generate a token into --admin-token-file if the file does not exist.
    #[arg(long)]
    create_admin_token: bool,

    /// How often agents send heartbeats.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "15s")]
    heartbeat_interval: Duration,

    /// How long without a heartbeat before a machine is offline.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "45s")]
    heartbeat_timeout: Duration,

    /// How long new enrollment tokens are valid unless the request says.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "1h")]
    enrollment_token_ttl: Duration,

    /// Where the server keeps repository mirrors, garbage collector roots
    /// for built systems, and the generated signing key.
    #[arg(long, env = "MNIX_STATE_DIR", default_value = "/var/lib/manage-nix")]
    state_dir: PathBuf,

    /// Nix secret key (`nix key generate-secret`) the server signs built
    /// systems with. Machines trust its public key in
    /// `services.manage-nix.agent.trustedPublicKeys`. Defaults to
    /// `<state dir>/signing-key`.
    #[arg(long, env = "MNIX_SIGNING_KEY_FILE")]
    signing_key_file: Option<PathBuf>,

    /// Generate a key into --signing-key-file if the file does not exist.
    #[arg(long)]
    create_signing_key: bool,

    /// The name of a generated signing key.
    #[arg(long, default_value = "manage-nix-1")]
    signing_key_name: String,

    /// Attic server to push built systems to, for example
    /// `https://cache.example.com/`. Without one, machines must be able to
    /// fetch the systems from somewhere else.
    #[arg(long, env = "MNIX_ATTIC_ENDPOINT", requires_all = ["attic_cache", "attic_token_file"])]
    attic_endpoint: Option<String>,

    /// The Attic cache to push to.
    #[arg(long, env = "MNIX_ATTIC_CACHE", requires = "attic_endpoint")]
    attic_cache: Option<String>,

    /// File containing an Attic token that may push to the cache.
    #[arg(long, env = "MNIX_ATTIC_TOKEN_FILE", requires = "attic_endpoint")]
    attic_token_file: Option<PathBuf>,

    /// How long a machine has, after a healthy activation, to reach the
    /// server over a new connection before it rolls back on its own.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "60s")]
    confirm_timeout: Duration,

    /// How long a machine may take from receiving a system (or coming
    /// online to receive it) to reporting the result.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "30m")]
    activation_timeout: Duration,

    /// How long fetching and building a deployment may take.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "1h")]
    build_timeout: Duration,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    match cli.command {
        None => println!(
            "mnix-server {} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            mnix_protocol::PROTOCOL_VERSION
        ),
        Some(Command::Migrate { database }) => {
            let store = mnix_store::connect(&database.database_url).await?;
            store.migrate().await?;
            println!("migrated {:?} database", store.backend());
        }
        Some(Command::Serve(args)) => serve(*args).await?,
    }
    Ok(())
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.heartbeat_interval >= Duration::from_secs(1)
            && args.heartbeat_timeout > args.heartbeat_interval,
        "--heartbeat-timeout must be longer than --heartbeat-interval (at least 1s)"
    );
    anyhow::ensure!(
        args.confirm_timeout >= Duration::from_secs(5),
        "--confirm-timeout must be at least 5s"
    );
    let url = &args.database.database_url;
    let backend = Backend::from_url(url)?;
    let _lock = match backend {
        Backend::Sqlite => sqlite_lock(url)?,
        Backend::Postgres => None,
    };
    let admin_token = admin_token(&args.admin_token_file, args.create_admin_token)?;
    let signing_key = signing_key(
        &args
            .signing_key_file
            .clone()
            .unwrap_or_else(|| args.state_dir.join("signing-key")),
        args.create_signing_key
            .then_some(args.signing_key_name.as_str()),
    )?;
    tracing::info!("signing systems with {}", signing_key.public_key());
    let attic = match (
        &args.attic_endpoint,
        &args.attic_cache,
        &args.attic_token_file,
    ) {
        (Some(endpoint), Some(cache), Some(token_file)) => {
            let token = std::fs::read_to_string(token_file).with_context(|| {
                format!("reading the Attic token from {}", token_file.display())
            })?;
            Some(build::Attic::configure(
                &args.state_dir,
                endpoint,
                cache,
                token.trim(),
            )?)
        }
        _ => None,
    };

    let store = mnix_store::connect(url).await?;
    store.migrate().await?;
    let organization = store
        .ensure_organization(DEFAULT_ORGANIZATION, OffsetDateTime::now_utc())
        .await?;

    let (shutdown_tx, shutdown) = tokio::sync::watch::channel(false);
    let state = Arc::new(app::AppState {
        store,
        organization,
        admin_token_hash: secret::hash(&admin_token),
        settings: app::Settings {
            heartbeat_interval: args.heartbeat_interval,
            heartbeat_timeout: args.heartbeat_timeout,
            default_token_ttl: args.enrollment_token_ttl,
            confirm_timeout: args.confirm_timeout,
            activation_timeout: args.activation_timeout,
            build_timeout: args.build_timeout,
        },
        shutdown,
        builder: build::Builder {
            state_dir: args.state_dir.clone(),
            signing_key,
            attic,
        },
        queue: tokio::sync::Notify::new(),
        activations: tokio::sync::watch::channel(0).0,
    });
    if backend == Backend::Sqlite {
        deploy::fail_interrupted_builds(&state).await?;
    }
    tokio::spawn(agent::reap_stale_sessions(state.clone()));
    tokio::spawn(deploy::run_worker(state.clone()));
    tokio::spawn(deploy::run_sweeper(state.clone()));

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("listening on {}", args.listen))?;
    tracing::info!(
        "mnix-server {} listening on {} ({:?} database)",
        env!("CARGO_PKG_VERSION"),
        args.listen,
        state.store.backend()
    );
    axum::serve(listener, app::router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            let _ = shutdown_tx.send(true);
        })
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("installing a SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// Reads the admin token, generating it first if asked to and the file is
/// missing.
fn admin_token(path: &Path, create: bool) -> anyhow::Result<String> {
    if create && !path.exists() {
        let token = secret::generate(secret::ADMIN_TOKEN_PREFIX);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        writeln!(file, "{token}")?;
        tracing::info!("generated an admin token in {}", path.display());
    }
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("reading the admin token from {}", path.display()))?;
    let token = token.trim().to_owned();
    anyhow::ensure!(
        token.len() >= 16,
        "the admin token in {} is shorter than 16 characters",
        path.display()
    );
    Ok(token)
}

/// Reads the signing key, generating it first if `create` names it and the
/// file is missing.
fn signing_key(path: &Path, create: Option<&str>) -> anyhow::Result<SecretKey> {
    if let Some(name) = create.filter(|_| !path.exists()) {
        let key = SecretKey::generate(name)?;
        write_private(path, &format!("{}\n", key.to_secret_string()))?;
        tracing::info!("generated a signing key in {}", path.display());
    }
    let key = std::fs::read_to_string(path)
        .with_context(|| format!("reading the signing key from {}", path.display()))?;
    SecretKey::parse(&key).with_context(|| format!("invalid signing key in {}", path.display()))
}

/// Creates a file readable only by its owner.
pub fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

/// SQLite supports one server instance: hold an exclusive lock on
/// `<database>.lock` for as long as the server runs.
fn sqlite_lock(url: &str) -> anyhow::Result<Option<File>> {
    let Some(path) = sqlite_path(url) else {
        return Ok(None);
    };
    let lock_path = PathBuf::from(format!("{path}.lock"));
    let file =
        File::create(&lock_path).with_context(|| format!("creating {}", lock_path.display()))?;
    file.try_lock().map_err(|_| {
        anyhow::anyhow!(
            "another mnix-server is using {path} (locked {}). SQLite supports one server \
             instance; use Postgres for replicas",
            lock_path.display()
        )
    })?;
    Ok(Some(file))
}

/// The file path in a `sqlite://` URL, or `None` for in-memory databases.
fn sqlite_path(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("sqlite:")?;
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let path = rest.split('?').next().unwrap_or(rest);
    (!path.is_empty() && path != ":memory:").then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn finds_the_sqlite_file() {
        assert_eq!(
            sqlite_path("sqlite:///var/lib/manage-nix/db.sqlite"),
            Some("/var/lib/manage-nix/db.sqlite")
        );
        assert_eq!(
            sqlite_path("sqlite://db.sqlite?mode=rwc"),
            Some("db.sqlite")
        );
        assert_eq!(sqlite_path("sqlite::memory:"), None);
        assert_eq!(sqlite_path("postgres://x"), None);
    }

    #[test]
    fn a_second_sqlite_server_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("db.sqlite").display());
        let first = sqlite_lock(&url).unwrap();
        assert!(first.is_some());
        let err = sqlite_lock(&url).unwrap_err().to_string();
        assert!(err.contains("use Postgres"), "{err}");
        drop(first);
        assert!(sqlite_lock(&url).unwrap().is_some());
    }

    #[test]
    fn signing_keys_are_generated_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("signing-key");
        assert!(signing_key(&path, None).is_err());
        let key = signing_key(&path, Some("mnix-test-1")).unwrap();
        assert!(key.public_key().starts_with("mnix-test-1:"));
        let again = signing_key(&path, Some("other-1")).unwrap();
        assert_eq!(again.public_key(), key.public_key());
        let mode = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
    }

    #[test]
    fn admin_tokens_are_generated_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin-token");
        assert!(admin_token(&path, false).is_err());
        let token = admin_token(&path, true).unwrap();
        assert!(token.starts_with(secret::ADMIN_TOKEN_PREFIX));
        assert_eq!(admin_token(&path, true).unwrap(), token);
        let mode = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
    }
}
