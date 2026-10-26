//! A small blocking client for the admin API.

use std::time::Duration;

use anyhow::Context;
use mnix_protocol::api::ErrorBody;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub struct Client {
    base: String,
    authorization: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(server: &str, token: String) -> Self {
        use ureq::tls::{RootCerts, TlsConfig};
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .into();
        Self {
            base: server.trim_end_matches('/').to_owned(),
            authorization: format!("Bearer {token}"),
            agent,
        }
    }

    pub fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let url = format!("{}{path}", self.base);
        let response = self
            .agent
            .get(&url)
            .header("Authorization", &self.authorization)
            .call()
            .with_context(|| format!("GET {url}"))?;
        decode(&url, response)
    }

    pub fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> anyhow::Result<T> {
        let url = format!("{}{path}", self.base);
        let response = self
            .agent
            .post(&url)
            .header("Authorization", &self.authorization)
            .send_json(body)
            .with_context(|| format!("POST {url}"))?;
        decode(&url, response)
    }
}

fn decode<T: DeserializeOwned>(
    url: &str,
    mut response: ureq::http::Response<ureq::Body>,
) -> anyhow::Result<T> {
    let status = response.status();
    if !status.is_success() {
        let reason = response
            .body_mut()
            .read_json::<ErrorBody>()
            .map_or_else(|_| status.to_string(), |body| body.error);
        anyhow::bail!("{reason} ({status})");
    }
    response
        .body_mut()
        .read_json()
        .with_context(|| format!("invalid response from {url}"))
}

/// Escapes a machine name or ID for use as a path segment. Valid names
/// need no escaping; anything else is percent-encoded.
pub fn escape(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_path_segments() {
        assert_eq!(escape("web-1.example_2"), "web-1.example_2");
        assert_eq!(escape("a/b c"), "a%2Fb%20c");
    }
}
