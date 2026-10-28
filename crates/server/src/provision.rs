//! Declarative setup: the repositories and machines the NixOS or nix-darwin
//! module declares (`--provision <file>`), applied at every start.
//!
//! Declared repositories are created or moved to their URL. Declared
//! machines are created, and get their repository, configuration, and tags.
//! A machine with an `enrollment_token_file` can enroll with the token in
//! it, which the machine's agent reads from the same secret: nobody has to
//! run `mnix machine add`. Such a token works once, like any other, but does
//! not expire. Nothing that is not declared is changed or removed.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::Context;
use mnix_store::{NewEnrollmentToken, Organization, Store};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{api, secret};

/// How long a declared token stays valid: for good, in practice.
const DECLARED_TOKEN_TTL: time::Duration = time::Duration::days(100 * 365);

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provision {
    #[serde(default)]
    pub repositories: BTreeMap<String, RepositorySpec>,
    #[serde(default)]
    pub machines: BTreeMap<String, MachineSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositorySpec {
    pub url: String,
    /// An SSH private key to fetch it with.
    #[serde(default)]
    pub ssh_key_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineSpec {
    /// A pre-shared enrollment token.
    #[serde(default)]
    pub enrollment_token_file: Option<PathBuf>,
    /// The name of a repository; needs `configuration`.
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub configuration: Option<String>,
    /// Replaces the machine's tags when set.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

impl Provision {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let json =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&json).with_context(|| format!("invalid {}", path.display()))
    }
}

/// Applies `provision`. Returns the SSH keys of the declared repositories,
/// by repository.
pub async fn apply(
    store: &dyn Store,
    organization: &Organization,
    provision: &Provision,
    now: OffsetDateTime,
) -> anyhow::Result<HashMap<Uuid, PathBuf>> {
    let org = organization.id;
    let mut repositories = HashMap::new();
    let mut ssh_keys = HashMap::new();
    let existing = store.repositories(org).await?;
    for (name, spec) in &provision.repositories {
        anyhow::ensure!(api::is_valid_name(name), "invalid repository name {name:?}");
        let id = match existing.iter().find(|r| r.name == *name) {
            Some(repo) => {
                if repo.url != spec.url {
                    store.set_repository_url(repo.id, &spec.url).await?;
                    tracing::info!("repository {name} moved to {}", spec.url);
                }
                repo.id
            }
            None => {
                let repo = store.create_repository(org, name, &spec.url, now).await?;
                tracing::info!("added the repository {name} ({})", spec.url);
                repo.id
            }
        };
        repositories.insert(name.as_str(), id);
        if let Some(key) = &spec.ssh_key_file {
            ssh_keys.insert(id, key.clone());
        }
    }

    let existing = store.machines(org).await?;
    for (name, spec) in &provision.machines {
        anyhow::ensure!(api::is_valid_name(name), "invalid machine name {name:?}");
        let machine = match existing.iter().find(|m| m.name == *name) {
            Some(machine) => machine.clone(),
            None => {
                tracing::info!("added the machine {name}");
                store.create_machine(org, name, now).await?
            }
        };
        if let (Some(file), None) = (&spec.enrollment_token_file, machine.enrolled_at) {
            let token = std::fs::read_to_string(file)
                .with_context(|| format!("reading the enrollment token of {name}"))?;
            let token = token.trim();
            anyhow::ensure!(
                token.len() >= 16,
                "the enrollment token of {name} in {} is shorter than 16 characters",
                file.display()
            );
            let created = store
                .create_enrollment_token(&NewEnrollmentToken {
                    machine_id: machine.id,
                    token_hash: secret::hash(token),
                    created_at: now,
                    expires_at: now + DECLARED_TOKEN_TTL,
                })
                .await;
            match created {
                Ok(()) => tracing::info!("{name} can enroll with its declared token"),
                // Declared on an earlier start.
                Err(mnix_store::Error::Conflict(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        match (&spec.repository, &spec.configuration) {
            (Some(repository), configuration) => {
                let repository_id = *repositories.get(repository.as_str()).with_context(|| {
                    format!("{name}: the repository {repository:?} is not declared")
                })?;
                let configuration = configuration.as_deref().unwrap_or(name);
                anyhow::ensure!(
                    api::is_valid_name(configuration),
                    "{name}: invalid configuration name {configuration:?}"
                );
                if machine.repository_id != Some(repository_id)
                    || machine.configuration.as_deref() != Some(configuration)
                {
                    store
                        .set_machine_configuration(machine.id, repository_id, configuration)
                        .await?;
                }
            }
            (None, Some(_)) => anyhow::bail!("{name}: a configuration needs a repository"),
            (None, None) => {}
        }
        if let Some(tags) = &spec.tags {
            let mut tags = tags.clone();
            tags.sort();
            tags.dedup();
            for tag in &tags {
                anyhow::ensure!(api::is_valid_name(tag), "{name}: invalid tag {tag:?}");
            }
            if machine.tags != tags {
                store.set_machine_tags(machine.id, &tags).await?;
            }
        }
    }
    Ok(ssh_keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mnix_store::Redemption;

    #[tokio::test]
    async fn declared_machines_enroll_with_their_token_once() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("db.sqlite").display());
        let store = mnix_store::connect(&url).await.unwrap();
        store.migrate().await.unwrap();
        let now = OffsetDateTime::now_utc();
        let org = store.ensure_organization("default", now).await.unwrap();
        let token = dir.path().join("token");
        std::fs::write(&token, "mnix_enroll_0123456789abcdef\n").unwrap();
        let provision: Provision = serde_json::from_value(serde_json::json!({
            "repositories": {
                "nix": {"url": "git@github.com:me/nix.git", "ssh_key_file": "/run/key"}
            },
            "machines": {
                "obsidian": {
                    "enrollment_token_file": token,
                    "repository": "nix",
                    "tags": ["desktop", "desktop"]
                },
                "fortress": {}
            }
        }))
        .unwrap();
        let keys = apply(&*store, &org, &provision, now).await.unwrap();
        // Applying again changes nothing and does not fail.
        assert_eq!(apply(&*store, &org, &provision, now).await.unwrap(), keys);
        assert_eq!(keys.values().collect::<Vec<_>>(), [Path::new("/run/key")]);

        let machines = store.machines(org.id).await.unwrap();
        let obsidian = machines.iter().find(|m| m.name == "obsidian").unwrap();
        assert_eq!(obsidian.configuration.as_deref(), Some("obsidian"));
        assert_eq!(obsidian.tags, ["desktop"]);
        assert!(machines.iter().any(|m| m.name == "fortress"));

        let redeem = |credential: &'static str| {
            let store = store.clone();
            async move {
                store
                    .redeem_enrollment_token(
                        &secret::hash("mnix_enroll_0123456789abcdef"),
                        &secret::hash(credential),
                        now,
                    )
                    .await
                    .unwrap()
            }
        };
        assert!(matches!(redeem("c1").await, Redemption::Enrolled { .. }));
        assert!(matches!(redeem("c2").await, Redemption::AlreadyUsed { .. }));
        // An enrolled machine's token is not declared again.
        apply(&*store, &org, &provision, now).await.unwrap();
        assert!(matches!(redeem("c3").await, Redemption::AlreadyUsed { .. }));

        let moved: Provision = serde_json::from_value(serde_json::json!({
            "repositories": {"nix": {"url": "https://example.com/nix.git"}},
            "machines": {"quartz": {"configuration": "quartz"}}
        }))
        .unwrap();
        assert!(apply(&*store, &org, &moved, now).await.is_err());
        let repos = store.repositories(org.id).await.unwrap();
        assert_eq!(repos[0].url, "https://example.com/nix.git");
    }
}
