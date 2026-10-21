//! The agent's configuration file, written by the NixOS module to
//! `/etc/manage-nix/agent.json`.
//!
//! The agent reads two copies. Trusted keys always come from the *running*
//! system, so a new configuration cannot vouch for itself. Health checks come
//! from the *target* system, because the new configuration defines what
//! healthy means for it.

use std::collections::BTreeMap;
use std::path::Path;
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
