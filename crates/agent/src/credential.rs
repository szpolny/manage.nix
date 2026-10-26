//! Enrollment and the machine credential the agent keeps in its state
//! directory.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use mnix_protocol::api::{EnrollRequest, EnrollResponse, ErrorBody};
use serde::{Deserialize, Serialize};

/// The credential file, relative to the state directory.
const CREDENTIAL_FILE: &str = "credential.json";
const CREDENTIAL_VERSION: u32 = 1;
const ENROLL_TIMEOUT: Duration = Duration::from_secs(30);

/// What enrollment gave this machine. Readable by root only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub version: u32,
    /// The server the machine enrolled with.
    pub server_url: String,
    pub machine_id: String,
    pub credential: String,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("server_url", &self.server_url)
            .field("machine_id", &self.machine_id)
            .finish_non_exhaustive()
    }
}

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(CREDENTIAL_FILE)
}

/// Loads the stored credential, if the machine has enrolled.
pub fn load(state_dir: &Path) -> anyhow::Result<Option<Credential>> {
    let path = path(state_dir);
    let json = match std::fs::read_to_string(&path) {
        Ok(json) => json,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let credential: Credential =
        serde_json::from_str(&json).with_context(|| format!("invalid {}", path.display()))?;
    anyhow::ensure!(
        credential.version == CREDENTIAL_VERSION,
        "unsupported credential version {} in {}",
        credential.version,
        path.display()
    );
    Ok(Some(credential))
}

/// Writes the credential atomically with mode 0600.
pub fn store(state_dir: &Path, credential: &Credential) -> anyhow::Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))?;
    let path = path(state_dir);
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(serde_json::to_string_pretty(credential)?.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Why enrollment did not work.
#[derive(Debug, thiserror::Error)]
pub enum EnrollError {
    /// The server refused the token; retrying with the same one is useless.
    #[error("the server rejected the enrollment token: {0}")]
    Rejected(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Exchanges a one-time enrollment token for a credential.
pub fn enroll(server_url: &str, token: &str) -> Result<Credential, EnrollError> {
    let url = format!(
        "{}{}",
        server_url.trim_end_matches('/'),
        mnix_protocol::ENROLL_PATH
    );
    let mut response = http_agent()
        .post(&url)
        .send_json(EnrollRequest {
            token: token.to_owned(),
        })
        .with_context(|| format!("POST {url}"))?;
    let status = response.status();
    if status.is_client_error() {
        let reason = response
            .body_mut()
            .read_json::<ErrorBody>()
            .map_or_else(|_| status.to_string(), |body| body.error);
        return Err(EnrollError::Rejected(reason));
    }
    if !status.is_success() {
        return Err(anyhow::anyhow!("POST {url}: {status}").into());
    }
    let enrolled: EnrollResponse = response
        .body_mut()
        .read_json()
        .with_context(|| format!("invalid response from {url}"))?;
    Ok(Credential {
        version: CREDENTIAL_VERSION,
        server_url: server_url.to_owned(),
        machine_id: enrolled.machine_id,
        credential: enrolled.credential,
    })
}

/// An HTTP client that trusts the system's CA certificates.
fn http_agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig};
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(ENROLL_TIMEOUT))
        .tls_config(
            TlsConfig::builder()
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into()
}

/// Reads an enrollment token from a file, ignoring surrounding whitespace.
pub fn read_token(path: &Path) -> anyhow::Result<String> {
    let token = std::fs::read_to_string(path)
        .with_context(|| format!("reading the enrollment token from {}", path.display()))?;
    let token = token.trim();
    anyhow::ensure!(!token.is_empty(), "{} is empty", path.display());
    Ok(token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_round_trip_privately() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        assert!(load(&state).unwrap().is_none());
        let credential = Credential {
            version: CREDENTIAL_VERSION,
            server_url: "https://mnix.example".to_owned(),
            machine_id: "m-1".to_owned(),
            credential: "mnix_cred_secret".to_owned(),
        };
        store(&state, &credential).unwrap();
        store(&state, &credential).unwrap();
        assert_eq!(load(&state).unwrap(), Some(credential.clone()));
        let mode = std::fs::metadata(path(&state))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!format!("{credential:?}").contains("secret"));
    }

    #[test]
    fn tokens_are_trimmed_and_must_not_be_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        std::fs::write(&file, "  mnix_enroll_abc\n").unwrap();
        assert_eq!(read_token(&file).unwrap(), "mnix_enroll_abc");
        std::fs::write(&file, "\n").unwrap();
        assert!(read_token(&file).is_err());
        assert!(read_token(&dir.path().join("missing")).is_err());
    }
}
