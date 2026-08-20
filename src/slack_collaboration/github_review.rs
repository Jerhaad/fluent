//! Host-owned GitHub pull-request and review boundary.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::work_model::{
    ArtifactRef, DurableTransitionMetadata, DurableTransitionOrigin, MergeCandidateMergeStatus,
    WorkModelError, WorkModelStore, validate_work_item_id,
};

/// The exact ready candidate authorized for publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubCandidate {
    pub work_item_id: String,
    pub candidate_id: String,
    pub commit: String,
    pub target_branch: String,
}

/// Durable identity of the single Fluent-created pull request for a Slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubPublication {
    pub slice_id: String,
    pub remote: String,
    pub head_branch: String,
    pub candidate: GitHubCandidate,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_history: Vec<GitHubCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request_url: Option<String>,
    #[serde(default)]
    pub feedback: Vec<GitHubReviewFeedback>,
    #[serde(default)]
    pub routed_feedback_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubPublishRequest {
    pub checkout: PathBuf,
    pub publication: GitHubPublication,
}

/// One stable review event returned by GitHub structured output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubReviewFeedback {
    pub id: String,
    pub kind: String,
    pub body: String,
    pub url: String,
    pub candidate_commit: String,
    #[serde(default)]
    pub resolved: bool,
}

/// Bounded external adapter. Implementations must reconcile the deterministic
/// head branch before creating a pull request after an uncertain result.
pub trait GitHubReviewClient {
    fn publish(
        &mut self,
        request: &GitHubPublishRequest,
        existing: Option<&GitHubPublication>,
    ) -> Result<String>;

    fn review_feedback(
        &mut self,
        checkout: &Path,
        publication: &GitHubPublication,
    ) -> Result<Vec<GitHubReviewFeedback>>;
}

/// A selected review request submitted to the existing candidate-rework owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateReworkRequest {
    pub id: String,
    pub slice_id: String,
    pub project_label: String,
    pub work_item_id: String,
    pub candidate_id: String,
    pub candidate_commit: String,
    pub authorized_by: String,
    pub feedback: Vec<GitHubReviewFeedback>,
}

pub trait CandidateReworkBoundary {
    fn submit_candidate_rework(&self, request: &CandidateReworkRequest) -> Result<()>;
}

/// Production adapter that turns selected feedback into a normal queued Write
/// Attempt. The persisted request is both immutable Writer input and the stable
/// idempotency identity for queue submission.
#[derive(Debug, Clone)]
pub struct LocalCandidateReworkBoundary {
    projects: BTreeMap<String, PathBuf>,
}

impl LocalCandidateReworkBoundary {
    pub fn new(projects: impl IntoIterator<Item = (String, PathBuf)>) -> Self {
        Self {
            projects: projects.into_iter().collect(),
        }
    }
}

impl CandidateReworkBoundary for LocalCandidateReworkBoundary {
    fn submit_candidate_rework(&self, request: &CandidateReworkRequest) -> Result<()> {
        validate_work_item_id(&request.work_item_id)?;
        if request.id.is_empty()
            || !request
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            bail!("candidate rework identity is not a safe path component");
        }
        let project_root = self.projects.get(&request.project_label).with_context(|| {
            format!(
                "candidate rework project {} is not configured",
                request.project_label
            )
        })?;
        let relative_path = format!(
            ".fluent/work/artifacts/{}/github-rework/{}.json",
            request.work_item_id, request.id
        );
        let path = project_root.join(&relative_path);
        let directory = path
            .parent()
            .expect("candidate rework artifact has a parent");
        fs::create_dir_all(directory)?;
        let bytes = serde_json::to_vec_pretty(request)?;
        match fs::read(&path) {
            Ok(existing) if existing == bytes => {}
            Ok(_) => bail!("candidate rework identity already has different content"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        }
        if !path.exists() {
            crate::atomic_write::atomic_write(&path, &bytes)?;
        }
        let artifact = ArtifactRef {
            producer_id: "github-review".into(),
            path: relative_path,
        };
        let metadata = DurableTransitionMetadata {
            origin: DurableTransitionOrigin::SlackThread,
            reason: Some(format!(
                "Project approver {} selected GitHub feedback for candidate {}",
                request.authorized_by, request.candidate_id
            )),
            evidence_ref: Some(format!("github-rework:{}", request.id)),
        };
        let instructions = candidate_rework_instructions(request);
        WorkModelStore::new(project_root).mutate_work_item_with_transition(
            &request.work_item_id,
            &metadata,
            |item| {
                if item.attempts.iter().any(|attempt| {
                    attempt
                        .tasks
                        .iter()
                        .any(|task| task.input_artifacts.contains(&artifact))
                }) {
                    return Ok(());
                }
                let candidate = item
                    .merge_candidates
                    .iter()
                    .find(|candidate| candidate.id == request.candidate_id)
                    .ok_or_else(|| WorkModelError::MergeCandidateNotFound {
                        candidate_id: request.candidate_id.clone(),
                    })?;
                if candidate.candidate_commit != request.candidate_commit
                    || candidate.merge_state.status != MergeCandidateMergeStatus::Pending
                    || candidate.ready_at.is_none()
                {
                    return Err(WorkModelError::MergeCandidateCorrectionIneligible {
                        candidate_id: request.candidate_id.clone(),
                        reason: "the request does not target the exact ready candidate".to_string(),
                    });
                }
                let completed_at = chrono::Utc::now().to_rfc3339();
                for pending in item.merge_candidates.iter_mut().filter(|candidate| {
                    candidate.merge_state.status == MergeCandidateMergeStatus::Pending
                }) {
                    pending.merge_state.status = MergeCandidateMergeStatus::Failed;
                    pending.merge_state.failure_reason = Some(format!(
                        "Superseded by authorized GitHub review request {} for candidate {}",
                        request.id, request.candidate_id
                    ));
                    pending.completed_at = Some(completed_at.clone());
                }

                let attempt_id = item.next_attempt_id();
                item.add_initial_attempt(attempt_id.clone())?;
                let attempt = item
                    .attempts
                    .iter_mut()
                    .find(|attempt| attempt.id == attempt_id)
                    .expect("new candidate rework Attempt exists");
                attempt.artifacts.push(artifact.clone());
                let write = attempt
                    .tasks
                    .first_mut()
                    .expect("new candidate rework Attempt has a Writer");
                write.instructions = Some(match write.instructions.take() {
                    Some(original) => format!("{original}\n\n{instructions}"),
                    None => instructions.clone(),
                });
                write.input_artifacts.push(artifact.clone());
                item.validate()?;
                Ok(())
            },
        )?;
        crate::queue::ensure_candidate_rework_dispatch(
            project_root,
            &request.work_item_id,
            &request.id,
            0,
        )?;
        Ok(())
    }
}

fn candidate_rework_instructions(request: &CandidateReworkRequest) -> String {
    let mut instructions = format!(
        "Address the project approver's selected GitHub review feedback for Merge Candidate {} at commit {}. Preserve the candidate and feedback identities recorded in the attached request.\n\nSelected feedback:",
        request.candidate_id, request.candidate_commit
    );
    for feedback in &request.feedback {
        instructions.push_str(&format!(
            "\n\n- {} ({}): {}\n  {}",
            feedback.id, feedback.kind, feedback.url, feedback.body
        ));
    }
    instructions
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitHubReviewRequestOutcome {
    Published(GitHubPublication),
    Failed {
        candidate_id: String,
        message: String,
    },
}

/// Production adapter around the host's authenticated `gh` executable.
#[derive(Debug, Clone)]
pub struct GhGitHubClient {
    executable: PathBuf,
}

impl Default for GhGitHubClient {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("gh"),
        }
    }
}

impl GhGitHubClient {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    fn gh(&self, checkout: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new(&self.executable)
            .current_dir(checkout)
            .args(args)
            .output()
            .with_context(|| format!("invoke host GitHub CLI {}", self.executable.display()))?;
        if !output.status.success() {
            let diagnostic = String::from_utf8_lossy(&output.stderr);
            bail!("GitHub CLI failed: {}", diagnostic.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn find_pull_request(&self, request: &GitHubPublishRequest) -> Result<Option<String>> {
        let repository = github_repository(&request.publication.remote)
            .context("configured GitHub remote is unsupported")?;
        let output = self.gh(
            &request.checkout,
            &[
                "pr",
                "list",
                "--repo",
                &repository,
                "--head",
                &request.publication.head_branch,
                "--state",
                "open",
                "--json",
                "url",
            ],
        )?;
        let rows: Vec<serde_json::Value> = serde_json::from_str(&output)
            .context("parse GitHub pull-request reconciliation output")?;
        Ok(rows
            .first()
            .and_then(|row| row.get("url"))
            .and_then(|url| url.as_str())
            .map(ToOwned::to_owned))
    }

    fn observed_remote_head(&self, checkout: &Path, branch: &str) -> Result<Option<String>> {
        let push_url = crate::git::run_stdout(
            checkout,
            &["remote", "get-url", "--push", "origin"],
            "inspect GitHub publication push remote",
        )?;
        let remote_ref = format!("refs/heads/{branch}");
        let output = crate::git::run_stdout(
            checkout,
            &["ls-remote", "--heads", "--", &push_url, &remote_ref],
            "inspect the current GitHub review head",
        )?;
        if output.trim().is_empty() {
            return Ok(None);
        }
        let mut rows = output.lines();
        let row = rows.next().expect("non-empty remote-head output");
        if rows.next().is_some() {
            bail!("Git returned multiple heads for the managed review branch");
        }
        let mut fields = row.split_whitespace();
        let commit = fields
            .next()
            .context("Git returned a head without a commit")?;
        let observed_ref = fields.next().context("Git returned a head without a ref")?;
        if fields.next().is_some()
            || observed_ref != remote_ref
            || commit.len() < 40
            || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("Git returned an invalid managed review head");
        }
        Ok(Some(commit.to_owned()))
    }
}

impl GitHubReviewClient for GhGitHubClient {
    fn publish(
        &mut self,
        request: &GitHubPublishRequest,
        existing: Option<&GitHubPublication>,
    ) -> Result<String> {
        let repository = github_repository(&request.publication.remote)
            .context("configured GitHub remote is unsupported")?;
        let origin = crate::git::run_stdout(
            &request.checkout,
            &["remote", "get-url", "origin"],
            "inspect GitHub publication remote",
        )?;
        if github_repository(&origin).as_deref() != Some(repository.as_str()) {
            bail!("Git origin does not match the configured github.com repository");
        }
        self.gh(
            &request.checkout,
            &["auth", "status", "--hostname", "github.com"],
        )?;
        let remote_ref = format!("refs/heads/{}", request.publication.head_branch);
        let expected = self
            .observed_remote_head(&request.checkout, &request.publication.head_branch)?
            .unwrap_or_default();
        let lease = format!("--force-with-lease={remote_ref}:{expected}");
        let refspec = format!("{}:{remote_ref}", request.publication.candidate.commit);
        crate::git::run(
            &request.checkout,
            &["push", &lease, "origin", &refspec],
            "publish the exact reviewed candidate",
        )?;
        if let Some(url) = existing.and_then(|record| record.pull_request_url.clone()) {
            return Ok(url);
        }
        if let Some(url) = self.find_pull_request(request)? {
            return Ok(url);
        }
        let title = format!("Review Slice {}", request.publication.slice_id);
        self.gh(
            &request.checkout,
            &[
                "pr",
                "create",
                "--repo",
                &repository,
                "--base",
                &request.publication.candidate.target_branch,
                "--head",
                &request.publication.head_branch,
                "--title",
                &title,
                "--body",
                "Fluent published this exact ready Merge Candidate for review.",
            ],
        )
    }

    fn review_feedback(
        &mut self,
        checkout: &Path,
        publication: &GitHubPublication,
    ) -> Result<Vec<GitHubReviewFeedback>> {
        let url = publication
            .pull_request_url
            .as_deref()
            .context("GitHub publication has no pull-request URL")?;
        let output = self.gh(checkout, &["pr", "view", url, "--json", "comments,reviews"])?;
        parse_feedback(&output, &publication.candidate.commit)
    }
}

fn github_repository(remote: &str) -> Option<String> {
    let path = remote
        .strip_prefix("https://github.com/")
        .or_else(|| remote.strip_prefix("git@github.com:"))?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    (!owner.is_empty() && !repo.is_empty() && parts.next().is_none())
        .then(|| format!("{owner}/{repo}"))
}

fn parse_feedback(output: &str, candidate_commit: &str) -> Result<Vec<GitHubReviewFeedback>> {
    let root: serde_json::Value =
        serde_json::from_str(output).context("parse GitHub review output")?;
    let mut feedback = Vec::new();
    for (field, kind) in [("comments", "comment"), ("reviews", "review")] {
        for row in root[field].as_array().into_iter().flatten() {
            let id = row["id"].as_str().context("GitHub feedback lacks id")?;
            let url = row["url"]
                .as_str()
                .or_else(|| row["htmlUrl"].as_str())
                .context("GitHub feedback lacks URL")?;
            feedback.push(GitHubReviewFeedback {
                id: id.to_owned(),
                kind: row["state"].as_str().unwrap_or(kind).to_ascii_lowercase(),
                body: row["body"].as_str().unwrap_or_default().to_owned(),
                url: url.to_owned(),
                candidate_commit: row["commit"]["oid"]
                    .as_str()
                    .unwrap_or(candidate_commit)
                    .to_owned(),
                resolved: row["isResolved"].as_bool().unwrap_or(false),
            });
        }
    }
    Ok(feedback)
}
