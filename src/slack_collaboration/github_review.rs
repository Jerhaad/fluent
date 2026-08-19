//! Host-owned GitHub pull-request and review boundary.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

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
        publication: &GitHubPublication,
    ) -> Result<Vec<GitHubReviewFeedback>>;
}

/// A selected review request submitted to the existing candidate-rework owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateReworkRequest {
    pub id: String,
    pub slice_id: String,
    pub work_item_id: String,
    pub candidate_id: String,
    pub candidate_commit: String,
    pub authorized_by: String,
    pub feedback: Vec<GitHubReviewFeedback>,
}

pub trait CandidateReworkBoundary {
    fn submit_candidate_rework(&self, request: &CandidateReworkRequest) -> Result<()>;
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
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
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
        let output = self.gh(
            &request.checkout,
            &[
                "pr",
                "list",
                "--repo",
                &request.publication.remote,
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
}

impl GitHubReviewClient for GhGitHubClient {
    fn publish(
        &mut self,
        request: &GitHubPublishRequest,
        existing: Option<&GitHubPublication>,
    ) -> Result<String> {
        self.gh(
            &request.checkout,
            &["auth", "status", "--hostname", "github.com"],
        )?;
        let refspec = format!(
            "{}:refs/heads/{}",
            request.publication.candidate.commit, request.publication.head_branch
        );
        crate::git::run(
            &request.checkout,
            &["push", "--force-with-lease", "origin", &refspec],
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
                &request.publication.remote,
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
        publication: &GitHubPublication,
    ) -> Result<Vec<GitHubReviewFeedback>> {
        let url = publication
            .pull_request_url
            .as_deref()
            .context("GitHub publication has no pull-request URL")?;
        let output = self.gh(
            Path::new("."),
            &["pr", "view", url, "--json", "comments,reviews"],
        )?;
        parse_feedback(&output, &publication.candidate.commit)
    }
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
