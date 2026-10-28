//! What a machine would get from a deploy: the commits of its repository
//! that its running system was not built from, and, on request, a preview
//! that builds the new system on the server and compares its closure with
//! the running one.
//!
//! Update checks fetch the repository at most once per [`FETCH_INTERVAL`].
//! Previews live in this server's memory; the latest one per machine is
//! kept until the server restarts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::Json;
use mnix_nix::StorePath;
use mnix_protocol::api::{
    Commit, CreatePreview, Preview, PreviewStatus, UpdateSummary, Updates as UpdatesView,
};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::app::{rfc3339, ApiError, AppState, State as AppStateRef};
use crate::build::{self, Log};
use crate::{api, closure};

/// How long a fetched repository counts as fresh for update checks.
const FETCH_INTERVAL: Duration = Duration::from_secs(60);
/// The most commits an update check lists.
const MAX_COMMITS: usize = 50;

/// Update checks' and previews' state in the server.
#[derive(Default)]
pub struct Tracker {
    /// When each repository was last fetched.
    fetched: tokio::sync::Mutex<HashMap<Uuid, OffsetDateTime>>,
    /// The latest preview of each machine.
    previews: Mutex<HashMap<Uuid, Preview>>,
}

/// A machine's repository and running commit, as far as they are known.
struct Subject {
    machine: mnix_store::Machine,
    repository: mnix_store::Repository,
    configuration: String,
    current_system: Option<String>,
    /// The commit the running system was built from.
    current_commit: Option<String>,
    /// The revision the running system was deployed from.
    deployed_rev: Option<String>,
}

async fn subject(state: &AppState, machine: mnix_store::Machine) -> Result<Subject, ApiError> {
    let (Some(repository_id), Some(configuration)) =
        (machine.repository_id, machine.configuration.clone())
    else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("{} has no configuration to compare with", machine.name),
        ));
    };
    let repository = state
        .store
        .repository(repository_id)
        .await?
        .ok_or_else(|| ApiError::not_found("the machine's repository"))?;
    let current_system = machine.facts.as_ref().map(|f| f.current_system.clone());
    let mut revision = None;
    if let Some(system) = &current_system {
        revision = state
            .store
            .system_revisions(state.organization.id, std::slice::from_ref(system))
            .await?
            .pop()
            .filter(|r| r.repository_id == Some(repository_id));
    }
    let current_commit = match &revision {
        Some(r) => Some(r.commit_id.clone()),
        // A clean `system.configurationRevision`: a full commit hash.
        None => machine
            .facts
            .as_ref()
            .and_then(|f| f.configuration_revision.clone())
            .filter(|c| is_commit(c)),
    };
    Ok(Subject {
        deployed_rev: revision.and_then(|r| r.rev),
        machine,
        repository,
        configuration,
        current_system,
        current_commit,
    })
}

impl Tracker {
    /// Makes the next update check fetch every repository.
    #[cfg(test)]
    pub async fn forget_fetches(&self) {
        self.fetched.lock().await.clear();
    }

    /// Fetches the repository unless it was fetched within
    /// [`FETCH_INTERVAL`]. Returns the mirror and when it was fetched.
    async fn fetch(
        &self,
        state: &AppState,
        repository: &mnix_store::Repository,
    ) -> anyhow::Result<(PathBuf, OffsetDateTime)> {
        let mut fetched = self.fetched.lock().await;
        let now = OffsetDateTime::now_utc();
        if let Some(&at) = fetched.get(&repository.id) {
            if now - at < FETCH_INTERVAL {
                return Ok((state.builder.mirror(repository.id), at));
            }
        }
        let mirror = state
            .builder
            .fetch(repository.id, &repository.url, &mut Log::discard())
            .await
            .with_context(|| format!("fetching {}", repository.name))?;
        fetched.insert(repository.id, now);
        Ok((mirror, now))
    }
}

/// What `rev` (or the default revision) has that the running commit lacks.
async fn check(
    state: &AppState,
    subject: &Subject,
    rev: Option<String>,
) -> anyhow::Result<UpdatesView> {
    let (mirror, fetched_at) = state.updates.fetch(state, &subject.repository).await?;
    let rev = match rev {
        Some(rev) => rev,
        None => default_rev(&mirror, subject.deployed_rev.as_deref()).await,
    };
    let commit = resolve(&mirror, &rev).await?;
    let mut view = UpdatesView {
        repository: subject.repository.name.clone(),
        rev,
        commit: commit.clone(),
        current_commit: subject.current_commit.clone(),
        behind: None,
        ahead: None,
        commits: Vec::new(),
        fetched_at: rfc3339(fetched_at),
    };
    let Some(current) = &subject.current_commit else {
        return Ok(view);
    };
    if !has_commit(&mirror, current).await {
        return Ok(view);
    }
    let counts = git(
        &mirror,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{current}...{commit}"),
        ],
    )
    .await?;
    let (ahead, behind) = parse_counts(&counts).context("unexpected git rev-list output")?;
    view.ahead = Some(ahead);
    view.behind = Some(behind);
    if behind > 0 {
        let log = git(
            &mirror,
            &[
                "log",
                "--format=%H%x1f%an%x1f%aI%x1f%s",
                &format!("--max-count={MAX_COMMITS}"),
                &format!("{current}..{commit}"),
            ],
        )
        .await?;
        view.commits = parse_log(&log);
    }
    Ok(view)
}

/// The branch the running system was deployed from, if it still exists;
/// otherwise the repository's default branch.
async fn default_rev(mirror: &Path, deployed: Option<&str>) -> String {
    if let Some(rev) = deployed.filter(|r| !is_commit(r) && *r != "HEAD") {
        let branch = format!("refs/heads/{rev}");
        if git(mirror, &["show-ref", "--verify", "--quiet", &branch])
            .await
            .is_ok()
        {
            return rev.to_owned();
        }
    }
    "HEAD".to_owned()
}

/// The commit `rev` names in the mirror.
async fn resolve(mirror: &Path, rev: &str) -> anyhow::Result<String> {
    let commit = git(
        mirror,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{rev}^{{commit}}"),
        ],
    )
    .await
    .map_err(|_| anyhow::anyhow!("{rev:?} is not a branch, tag, or commit of the repository"))?;
    let commit = commit.trim().to_owned();
    anyhow::ensure!(is_commit(&commit), "unexpected commit {commit:?}");
    Ok(commit)
}

async fn has_commit(mirror: &Path, commit: &str) -> bool {
    git(mirror, &["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .await
        .is_ok()
}

/// Runs a read-only `git` command in the mirror and returns its stdout.
async fn git(mirror: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = build::git()
        .arg("-C")
        .arg(mirror)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .context("running git")?;
    anyhow::ensure!(
        out.status.success(),
        "git {}: {}",
        args.first().unwrap_or(&""),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn is_commit(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// `git rev-list --left-right --count a...b`: commits only in `a`, then
/// only in `b`.
fn parse_counts(out: &str) -> Option<(u64, u64)> {
    let mut parts = out.split_whitespace().map(str::parse);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(left)), Some(Ok(right)), None) => Some((left, right)),
        _ => None,
    }
}

fn parse_log(out: &str) -> Vec<Commit> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(4, '\u{1f}');
            Some(Commit {
                commit: fields.next()?.to_owned(),
                author: fields.next()?.to_owned(),
                date: fields.next()?.to_owned(),
                summary: fields.next()?.to_owned(),
            })
        })
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct UpdatesQuery {
    rev: Option<String>,
}

/// `GET /api/machines/:machine/updates?rev=<rev>`
pub async fn machine_updates(
    State(state): State<AppStateRef>,
    UrlPath(machine): UrlPath<String>,
    Query(query): Query<UpdatesQuery>,
) -> Result<Json<UpdatesView>, ApiError> {
    if let Some(rev) = &query.rev {
        crate::deploy::validate_rev(rev)?;
    }
    let machine = api::find_machine(&state, &machine).await?;
    let subject = subject(&state, machine).await?;
    let updates = check(&state, &subject, query.rev).await.map_err(|e| {
        tracing::warn!("{}: checking for updates: {e:#}", subject.machine.name);
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}"))
    })?;
    Ok(Json(updates))
}

/// `GET /api/updates`: each configured machine's update check against its
/// default revision. Failures are reported per machine.
pub async fn all_updates(
    State(state): State<AppStateRef>,
) -> Result<Json<Vec<UpdateSummary>>, ApiError> {
    let machines = state.store.machines(state.organization.id).await?;
    let mut summaries = Vec::new();
    for machine in machines {
        if machine.repository_id.is_none() || machine.configuration.is_none() {
            continue;
        }
        let name = machine.name.clone();
        let summary = match subject(&state, machine).await {
            Ok(subject) => match check(&state, &subject, None).await {
                Ok(u) => UpdateSummary {
                    machine: name,
                    rev: Some(u.rev),
                    commit: Some(u.commit),
                    behind: u.behind,
                    error: None,
                },
                Err(e) => UpdateSummary {
                    machine: name,
                    rev: None,
                    commit: None,
                    behind: None,
                    error: Some(format!("{e:#}")),
                },
            },
            Err(e) => UpdateSummary {
                machine: name,
                rev: None,
                commit: None,
                behind: None,
                error: Some(e.message),
            },
        };
        summaries.push(summary);
    }
    Ok(Json(summaries))
}

/// `GET /api/machines/:machine/updates/preview`
pub async fn get_preview(
    State(state): State<AppStateRef>,
    UrlPath(machine): UrlPath<String>,
) -> Result<Json<Preview>, ApiError> {
    let machine = api::find_machine(&state, &machine).await?;
    let preview = state
        .updates
        .previews
        .lock()
        .expect("previews lock")
        .get(&machine.id)
        .cloned();
    preview
        .map(Json)
        .ok_or_else(|| ApiError::not_found("preview"))
}

/// `POST /api/machines/:machine/updates/preview`: starts building `rev`
/// (or the default revision) for the machine.
pub async fn create_preview(
    State(state): State<AppStateRef>,
    UrlPath(machine): UrlPath<String>,
    Json(request): Json<CreatePreview>,
) -> Result<(StatusCode, Json<Preview>), ApiError> {
    if let Some(rev) = &request.rev {
        crate::deploy::validate_rev(rev)?;
    }
    let machine = api::find_machine(&state, &machine).await?;
    let subject = subject(&state, machine).await?;
    let Some(from) = subject.current_system.clone() else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("{} has not reported its system yet", subject.machine.name),
        ));
    };
    let preview = Preview {
        status: PreviewStatus::Building,
        rev: request.rev.clone().unwrap_or_default(),
        commit: None,
        from,
        to: None,
        diff: None,
        message: None,
        started_at: rfc3339(OffsetDateTime::now_utc()),
        finished_at: None,
    };
    {
        let mut previews = state.updates.previews.lock().expect("previews lock");
        if previews
            .get(&subject.machine.id)
            .is_some_and(|p| p.status == PreviewStatus::Building)
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                format!("a preview for {} is already building", subject.machine.name),
            ));
        }
        previews.insert(subject.machine.id, preview.clone());
    }
    tracing::info!("{}: previewing an update", subject.machine.name);
    let task_state = state.clone();
    tokio::spawn(async move {
        let machine_id = subject.machine.id;
        let timeout = task_state.settings.build_timeout;
        let result = tokio::time::timeout(timeout, run_preview(&task_state, &subject, request.rev))
            .await
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "the build did not finish within {}",
                    humantime::format_duration(timeout)
                ))
            });
        let mut previews = task_state.updates.previews.lock().expect("previews lock");
        let Some(preview) = previews.get_mut(&machine_id) else {
            return;
        };
        preview.finished_at = Some(rfc3339(OffsetDateTime::now_utc()));
        match result {
            Ok(()) => preview.status = PreviewStatus::Succeeded,
            Err(e) => {
                tracing::warn!("{}: preview failed: {e:#}", subject.machine.name);
                preview.status = PreviewStatus::Failed;
                preview.message = Some(format!("{e:#}"));
            }
        }
    });
    Ok((StatusCode::ACCEPTED, Json(preview)))
}

/// Fetches, builds, and compares, recording each step in the preview.
async fn run_preview(
    state: &AppState,
    subject: &Subject,
    rev: Option<String>,
) -> anyhow::Result<()> {
    let id = subject.machine.id;
    let update = |change: &dyn Fn(&mut Preview)| {
        if let Some(preview) = state
            .updates
            .previews
            .lock()
            .expect("previews lock")
            .get_mut(&id)
        {
            change(preview);
        }
    };
    let (mirror, _) = state.updates.fetch(state, &subject.repository).await?;
    let rev = match rev {
        Some(rev) => rev,
        None => default_rev(&mirror, subject.deployed_rev.as_deref()).await,
    };
    let commit = resolve(&mirror, &rev).await?;
    update(&|p| {
        p.rev = rev.clone();
        p.commit = Some(commit.clone());
    });
    let os = subject
        .machine
        .facts
        .as_ref()
        .map(|f| f.os)
        .unwrap_or_default();
    let installable = build::installable(
        &mirror,
        &commit,
        &os.flake_attribute(&subject.configuration),
    )?;
    let out_link = state
        .builder
        .state_dir
        .join("gcroots")
        .join(format!("preview-{id}"));
    let to = state
        .builder
        .nix_build(&installable, &out_link, &mut Log::discard())
        .await?;
    update(&|p| p.to = Some(to.to_string()));
    let from = StorePath::parse(subject.current_system.as_deref().unwrap_or_default())?;
    let diff = closure::diff(&from, &to).await?;
    update(&|p| p.diff = Some(diff.clone()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rev_list_counts_are_left_then_right() {
        assert_eq!(parse_counts("2\t5\n"), Some((2, 5)));
        assert_eq!(parse_counts("0 0"), Some((0, 0)));
        assert_eq!(parse_counts(""), None);
        assert_eq!(parse_counts("1"), None);
        assert_eq!(parse_counts("1 2 3"), None);
    }

    #[test]
    fn log_lines_split_on_unit_separators() {
        let commits = parse_log(
            "abc\u{1f}Ada\u{1f}2026-10-28T10:00:00+01:00\u{1f}fix: a | b \u{1f} c\nbroken\n",
        );
        assert_eq!(
            commits,
            [Commit {
                commit: "abc".to_owned(),
                author: "Ada".to_owned(),
                date: "2026-10-28T10:00:00+01:00".to_owned(),
                summary: "fix: a | b \u{1f} c".to_owned(),
            }]
        );
    }
}
