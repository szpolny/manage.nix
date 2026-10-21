//! Health checks run after activation.
//!
//! All checks run in rounds until a round passes or the deadline is reached.
//! A unit that is restarting, an endpoint that is not up yet, or a script
//! that fails all count as "not healthy yet" until the deadline.

use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::HealthChecks;

/// How long one HTTP request may take.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one custom command may run.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// `PATH` for custom commands: the newly activated system's tools.
const COMMAND_PATH: &str = "/run/wrappers/bin:/run/current-system/sw/bin";

/// Runs every check once. Returns a description of each failing check.
pub fn run_round(checks: &HealthChecks) -> Vec<String> {
    let units = checks
        .systemd_units
        .iter()
        .map(|unit| (format!("systemd unit {unit}"), systemd_unit(unit)));
    let http = checks
        .http
        .iter()
        .map(|url| (format!("http {url}"), http(url)));
    let commands = checks
        .commands
        .iter()
        .map(|(name, exe)| (format!("command {name}"), command(exe)));
    units
        .chain(http)
        .chain(commands)
        .filter_map(|(check, result)| result.err().map(|why| format!("{check}: {why}")))
        .collect()
}

/// Calls `round` every `interval` until it reports no failures or `timeout`
/// has passed. Returns the failures of the last round.
pub fn wait_until_healthy(
    timeout: Duration,
    interval: Duration,
    mut round: impl FnMut() -> Vec<String>,
) -> Result<(), Vec<String>> {
    let deadline = Instant::now() + timeout;
    loop {
        let failures = round();
        if failures.is_empty() {
            return Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(failures);
        }
        tracing::info!(failing = ?failures, "not healthy yet");
        thread::sleep(interval.min(deadline - now));
    }
}

fn systemd_unit(unit: &str) -> Result<(), String> {
    let output = Command::new("systemctl")
        .args(["show", "--property=ActiveState", "--value", "--", unit])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running systemctl: {e}"))?;
    let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.status.success() && state == "active" {
        Ok(())
    } else if state.is_empty() {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    } else {
        Err(format!("is {state}"))
    }
}

fn http(url: &str) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let response = agent.get(url).call().map_err(|e| e.to_string())?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("returned {status}"))
    }
}

fn command(exe: &str) -> Result<(), String> {
    let mut child = Command::new(exe)
        .env_clear()
        .env("PATH", COMMAND_PATH)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("starting {exe}: {e}"))?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("exited with {status}")),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {}s", COMMAND_TIMEOUT.as_secs()));
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn passes_once_a_round_succeeds() {
        let mut rounds = 0;
        let result = wait_until_healthy(Duration::from_secs(5), Duration::from_millis(1), || {
            rounds += 1;
            if rounds < 3 {
                vec!["not yet".into()]
            } else {
                vec![]
            }
        });
        assert_eq!(result, Ok(()));
        assert_eq!(rounds, 3);
    }

    #[test]
    fn fails_with_the_last_round_after_the_deadline() {
        let mut rounds = 0;
        let result = wait_until_healthy(Duration::ZERO, Duration::from_millis(1), || {
            rounds += 1;
            vec![format!("round {rounds}")]
        });
        assert_eq!(result, Err(vec!["round 1".to_owned()]));
    }

    #[test]
    fn commands_report_their_exit_status() {
        let checks = HealthChecks {
            commands: BTreeMap::from([
                ("ok".into(), "/usr/bin/true".into()),
                ("bad".into(), "/usr/bin/false".into()),
                ("missing".into(), "/nonexistent/check".into()),
            ]),
            ..HealthChecks::default()
        };
        // `true` and `false` live in /usr/bin on macOS and FHS Linux, but not
        // in the Nix build sandbox; skip there.
        if !std::path::Path::new("/usr/bin/true").exists() {
            return;
        }
        let failures = run_round(&checks);
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(failures[0].starts_with("command bad: exited with"));
        assert!(failures[1].starts_with("command missing: starting"));
    }

    #[test]
    fn http_reports_status_and_connection_errors() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for status in ["200 OK", "503 Service Unavailable"] {
                let (mut conn, _) = listener.accept().unwrap();
                let mut buf = [0; 1024];
                let _ = conn.read(&mut buf).unwrap();
                write!(
                    conn,
                    "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                )
                .unwrap();
            }
        });
        let url = format!("http://{addr}/");
        assert_eq!(http(&url), Ok(()));
        assert_eq!(http(&url), Err("returned 503 Service Unavailable".into()));
        server.join().unwrap();
        // Nothing listens there any more.
        assert!(http(&url).is_err());
    }
}
