//! Read-only system service snapshots, collected with machine facts.
use mnix_protocol::{MachineService, ServiceSnapshot};
use std::process::{Command, Stdio};

pub fn collect() -> ServiceSnapshot {
    let result = inspect();
    ServiceSnapshot {
        refreshed_at: humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string(),
        error: result.as_ref().err().map(ToString::to_string),
        services: result.unwrap_or_default(),
    }
}

#[cfg(not(target_os = "macos"))]
fn inspect() -> anyhow::Result<Vec<MachineService>> {
    let out = Command::new("systemctl")
        .args([
            "list-units",
            "--type=service",
            "--all",
            "--no-pager",
            "--output=json",
        ])
        .env("SYSTEMD_BUS_TIMEOUT", "10s")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    anyhow::ensure!(out.status.success(), "System service inspection failed");
    parse_systemd(&out.stdout)
}

#[cfg(target_os = "macos")]
fn inspect() -> anyhow::Result<Vec<MachineService>> {
    let out = Command::new("/bin/launchctl")
        .arg("list")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    anyhow::ensure!(out.status.success(), "System service inspection failed");
    parse_launchd(&String::from_utf8(out.stdout)?)
}

#[cfg(any(test, not(target_os = "macos")))]
fn parse_systemd(bytes: &[u8]) -> anyhow::Result<Vec<MachineService>> {
    #[derive(serde::Deserialize)]
    struct Unit {
        unit: String,
        description: String,
        load: String,
        active: String,
        sub: String,
    }
    let units: Vec<Unit> = serde_json::from_slice(bytes)?;
    let mut services: Vec<_> = units
        .into_iter()
        .map(|u| MachineService {
            name: u.unit,
            description: u.description,
            state: match u.active.as_str() {
                "failed" => "failed",
                "inactive" => "stopped",
                "active" if u.sub == "running" => "running",
                _ => "other",
            }
            .into(),
            active_state: u.active,
            sub_state: u.sub,
            load_state: u.load,
        })
        .collect();
    services.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(services)
}

#[cfg(any(test, target_os = "macos"))]
fn parse_launchd(text: &str) -> anyhow::Result<Vec<MachineService>> {
    let mut services = Vec::new();
    anyhow::ensure!(
        text.lines().next() == Some("PID\tStatus\tLabel"),
        "Invalid launchctl response"
    );
    for line in text.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().collect();
        anyhow::ensure!(fields.len() == 3, "Invalid launchctl service");
        let exit: i32 = fields[1].parse()?;
        let state = if fields[0] != "-" {
            "running"
        } else if exit != 0 {
            "failed"
        } else {
            "stopped"
        };
        services.push(MachineService {
            name: fields[2].into(),
            description: String::new(),
            state: state.into(),
            active_state: state.into(),
            sub_state: format!("PID: {}; last exit status: {exit}", fields[0]),
            load_state: "loaded".into(),
        });
    }
    services.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(services)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn systemd_states_and_details() {
        let services = parse_systemd(br#"[
            {"unit":"a.service","description":"A","load":"loaded","active":"active","sub":"running"},
            {"unit":"b.service","description":"B","load":"loaded","active":"inactive","sub":"dead"},
            {"unit":"c.service","description":"C","load":"error","active":"failed","sub":"failed"},
            {"unit":"d.service","description":"D","load":"loaded","active":"active","sub":"exited"}
        ]"#).unwrap();
        assert_eq!(
            services
                .iter()
                .map(|s| s.state.as_str())
                .collect::<Vec<_>>(),
            ["running", "stopped", "failed", "other"]
        );
        assert_eq!(services[2].load_state, "error");
        assert!(parse_systemd(b"invalid").is_err());
        assert!(parse_systemd(b"[]").unwrap().is_empty());
    }
    #[test]
    fn launchd_states() {
        let services = parse_launchd("PID\tStatus\tLabel\n12\t0\ta\n-\t0\tb\n-\t1\tc\n").unwrap();
        assert_eq!(
            services
                .iter()
                .map(|s| s.state.as_str())
                .collect::<Vec<_>>(),
            ["running", "stopped", "failed"]
        );
        assert!(parse_launchd("").is_err());
    }
}
