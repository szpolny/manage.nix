//! The agent's configuration file, written by the NixOS and nix-darwin
//! modules to `/etc/manage-nix/agent.json`.
//!
//! The agent reads two copies. Trusted keys and servers always come from the
//! *running* system, so a new configuration cannot vouch for itself. Health
//! checks come from the *target* system, because the new configuration
//! defines what healthy means for it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

/// Where the NixOS module installs the configuration, relative to `/` or to
/// a system's top-level store path.
pub const CONFIG_PATH: &str = "etc/manage-nix/agent.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Format version of this file.
    pub version: u32,
    /// Public keys (`name:base64`) whose signature on a system's top-level
    /// store path allows the agent to activate it.
    #[serde(default)]
    pub trusted_public_keys: Vec<String>,
    #[serde(default)]
    pub health_checks: HealthChecks,
    /// The control planes `mnix-agent run` connects to, all at once.
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
}

/// One control plane the agent connects to.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Names the server's credential and activations in the state
    /// directory: letters, digits, `-`, and `_`.
    pub name: String,
    /// The base URL, `https://...` or `http://...`.
    pub url: String,
    /// The one-time (or pre-shared) enrollment token, read only until the
    /// machine has enrolled with this server.
    #[serde(default)]
    pub enrollment_token_file: Option<PathBuf>,
    /// Whether this server may have the machine build a system from source
    /// it sends ([`mnix_protocol::ActivationTarget::Build`]). Building and
    /// activating a configuration runs its code as root, so this trusts the
    /// server as much as root.
    #[serde(default)]
    pub allow_builds: bool,
}

impl ServerConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            is_valid_name(&self.name),
            "invalid server name {:?}: use letters, digits, '-', and '_'",
            self.name
        );
        anyhow::ensure!(
            self.url.starts_with("https://") || self.url.starts_with("http://"),
            "server {}: URL {:?} must start with https:// or http://",
            self.name,
            self.url
        );
        Ok(())
    }
}

/// A server name, safe as a file name.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthChecks {
    /// How long checks may take to pass after activation.
    pub timeout_secs: u64,
    /// Pause between rounds of checks.
    pub interval_secs: u64,
    /// systemd units that must be active.
    #[serde(default)]
    pub systemd_units: Vec<String>,
    /// URLs that must answer a GET with a 2xx status.
    #[serde(default)]
    pub http: Vec<String>,
    /// Executables (by check name) that must exit with status 0.
    #[serde(default)]
    pub commands: BTreeMap<String, String>,
}

impl Default for HealthChecks {
    fn default() -> Self {
        Self {
            timeout_secs: 60,
            interval_secs: 2,
            systemd_units: Vec::new(),
            http: Vec::new(),
            commands: BTreeMap::new(),
        }
    }
}

impl HealthChecks {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs.max(1))
    }

    pub fn is_empty(&self) -> bool {
        self.systemd_units.is_empty() && self.http.is_empty() && self.commands.is_empty()
    }
}

/// The only format version this agent understands.
pub const CONFIG_VERSION: u32 = 1;

impl AgentConfig {
    pub fn parse(json: &str) -> anyhow::Result<Self> {
        let config: Self = serde_json::from_str(json)?;
        anyhow::ensure!(
            config.version == CONFIG_VERSION,
            "unsupported agent configuration version {} (expected {CONFIG_VERSION})",
            config.version
        );
        Ok(config)
    }

    /// Reads the configuration installed under `root` (`/` or a system's
    /// top-level store path). Returns `None` when there is none.
    pub fn load_from(root: &Path) -> anyhow::Result<Option<Self>> {
        let path = root.join(CONFIG_PATH);
        match std::fs::read_to_string(&path) {
            Ok(json) => Self::parse(&json)
                .with_context(|| format!("invalid agent configuration {}", path.display()))
                .map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_module_output() {
        let config = AgentConfig::parse(
            r#"{
              "version": 1,
              "trusted_public_keys": ["mnix-1:AAAA"],
              "health_checks": {
                "timeout_secs": 30,
                "interval_secs": 1,
                "systemd_units": ["nginx.service"],
                "http": ["http://127.0.0.1/"],
                "commands": {"db": "/nix/store/x-db"}
              }
            }"#,
        )
        .unwrap();
        assert_eq!(config.trusted_public_keys, ["mnix-1:AAAA"]);
        assert_eq!(config.health_checks.timeout(), Duration::from_secs(30));
        assert_eq!(config.health_checks.commands["db"], "/nix/store/x-db");
        assert!(!config.health_checks.is_empty());
        assert!(config.servers.is_empty());
    }

    #[test]
    fn parses_servers() {
        let config = AgentConfig::parse(
            r#"{
              "version": 1,
              "servers": [
                {"name": "obsidian", "url": "http://100.64.0.1:8080", "allow_builds": true,
                 "enrollment_token_file": "/run/agenix/token"},
                {"name": "quartz", "url": "https://quartz.example"}
              ]
            }"#,
        )
        .unwrap();
        assert_eq!(config.servers.len(), 2);
        assert!(config.servers[0].allow_builds);
        assert!(!config.servers[1].allow_builds);
        assert_eq!(config.servers[1].enrollment_token_file, None);
        for server in &config.servers {
            server.validate().unwrap();
        }
        let bad = |name: &str, url: &str| ServerConfig {
            name: name.to_owned(),
            url: url.to_owned(),
            enrollment_token_file: None,
            allow_builds: false,
        };
        assert!(bad("../x", "https://x").validate().is_err());
        assert!(bad("", "https://x").validate().is_err());
        assert!(bad("x", "x.example").validate().is_err());
    }

    #[test]
    fn rejects_unknown_versions_and_fields() {
        assert!(AgentConfig::parse(r#"{"version": 2}"#).is_err());
        assert!(AgentConfig::parse(r#"{"version": 1, "surprise": true}"#).is_err());
        assert!(AgentConfig::parse(r#"{"version": 1}"#)
            .unwrap()
            .health_checks
            .is_empty());
    }

    #[test]
    fn missing_file_is_none() {
        let dir = std::env::temp_dir().join(format!("mnix-config-{}", std::process::id()));
        assert_eq!(AgentConfig::load_from(&dir).unwrap(), None);
    }
}
