//! Run cheap, host-owned checks before Fluent spends a model-review cycle.
//! Recheck corrective candidates from their original base and admit a history
//! rewrite only when it repairs metadata without changing the candidate tree.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

use crate::work_model::{
    CANDIDATE_METADATA_REPAIR_ROLE, WriterCompletionContract, WriterCompletionRequirementKind,
};

pub const CANDIDATE_GATE_SCHEMA_VERSION: u32 = 4;
pub const CANDIDATE_GATE_FILE: &str = "candidate-gate.json";
const CANDIDATE_GATE_MAX_BYTES: u64 = 256 * 1024;
const MAX_PROJECT_CHECK_LOG_BYTES: u64 = 256 * 1024;
pub(crate) const COMMIT_PRESENTATION_ADVISORY_MARKER: &str =
    "# fluent-policy: commit-message-presentation-advisory-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CandidateGatePhase {
    Writer,
    FinalTester,
    Learner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CandidateGateDisposition {
    Passed,
    Rejected,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CandidateCheckStatus {
    Passed,
    Failed,
    Skipped,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateCheckEvidence {
    pub name: String,
    pub status: CandidateCheckStatus,
    pub diagnostics: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateGateEvidence {
    pub schema_version: u32,
    pub phase: CandidateGatePhase,
    pub disposition: CandidateGateDisposition,
    pub base_commit: String,
    pub candidate_commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tester_results_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_rewrite: Option<CandidateMetadataRewriteEvidence>,
    pub checks: Vec<CandidateCheckEvidence>,
}

/// Immutable candidate-gate evidence that the current project policy has
/// retired without weakening any other candidate check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetiredCommitStyleGate {
    pub candidate_tree: String,
    pub gate_digest: String,
    pub hook_log_path: String,
    pub hook_log_digest: String,
    pub findings: Vec<String>,
    pub policy_commit: String,
}

/// Host-verified provenance for a pre-review rewrite that changed only commit
/// metadata while preserving the rejected candidate's exact tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateMetadataRewriteEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_gate_path: Option<String>,
    #[serde(default)]
    pub authorization: CandidateMetadataRewriteAuthorization,
    pub original_base_commit: String,
    pub rejected_candidate_commit: String,
    pub replacement_candidate_commit: String,
    pub candidate_tree: String,
}

/// Exact host-owned authority for repairing a metadata rewrite that preserved
/// the candidate tree but accidentally removed the protected base from its
/// ancestry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateAncestryRepairAuthorization {
    pub protected_base_commit: String,
    pub rejected_candidate_commit: String,
    pub candidate_commit: String,
    pub candidate_tree: String,
    pub prior_gate_path: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CandidateMetadataRewriteAuthorization {
    #[default]
    RejectedMetadataGate,
    WriterCompletionRepair,
    ReviewFinding,
}

/// Host-validated authority for a tree-identical metadata rewrite authorized by
/// a completed review finding rather than a rejected candidate gate.
///
/// The Writer still supplies the replacement commit. The candidate gate uses
/// this context to verify that the replacement rewrites only commit metadata
/// after the protected base and keeps the source candidate's exact tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedMetadataRewriteContext {
    pub source_candidate_commit: String,
    pub protected_base_commit: String,
    pub candidate_tree: String,
}

pub struct CandidateGateRequest<'a> {
    pub project_root: &'a Path,
    pub workspace: &'a Path,
    pub work_item_id: &'a str,
    pub attempt_id: &'a str,
    pub task_id: Option<&'a str>,
    pub phase: CandidateGatePhase,
    pub base_commit: &'a str,
    pub candidate_commit: &'a str,
    pub completion_contract: Option<&'a WriterCompletionContract>,
    pub allow_tree_identical_metadata_rewrite: bool,
    pub metadata_rewrite_authorization: Option<&'a AuthorizedMetadataRewriteContext>,
    pub tester_results_path: Option<&'a Path>,
    /// Exact candidate-bound commands whose failed canonical Tester result was
    /// superseded by accepted host evidence and a passing targeted Tests review.
    pub accepted_tester_commands: &'a [String],
    pub artifact_path: &'a Path,
}

pub fn run_candidate_gate(request: CandidateGateRequest<'_>) -> Result<CandidateGateEvidence> {
    run_candidate_gate_with_prior(request, None)
}

pub(crate) fn run_candidate_gate_with_prior(
    request: CandidateGateRequest<'_>,
    prior_candidate_gate_path: Option<&Path>,
) -> Result<CandidateGateEvidence> {
    run_candidate_gate_with_prior_and_role(request, prior_candidate_gate_path, None)
}

/// Run the post-Writer candidate gate with the Writer role that produced the
/// candidate.  Metadata-recovery Writers must stay inside the audited,
/// tree-identical rewrite scope even when their replacement is a descendant of
/// the rejected candidate; ordinary Writers retain the append-only path.
pub(crate) fn run_candidate_gate_with_prior_and_role(
    request: CandidateGateRequest<'_>,
    prior_candidate_gate_path: Option<&Path>,
    writer_role: Option<&str>,
) -> Result<CandidateGateEvidence> {
    let identity = check_candidate_identity(&request, prior_candidate_gate_path, writer_role)?;
    let mut checks = vec![
        identity.check,
        check_whitespace(&request, &identity.effective_base_commit)?,
        check_commit_metadata(&request, &identity.effective_base_commit)?,
    ];
    checks.extend(check_behavior_references(&request)?);
    checks.push(run_project_check(
        &request,
        &identity.effective_base_commit,
    )?);

    let disposition = if checks
        .iter()
        .any(|check| check.status == CandidateCheckStatus::Error)
    {
        CandidateGateDisposition::Blocked
    } else if checks
        .iter()
        .any(|check| check.status == CandidateCheckStatus::Failed)
    {
        CandidateGateDisposition::Rejected
    } else {
        CandidateGateDisposition::Passed
    };
    let evidence = CandidateGateEvidence {
        schema_version: CANDIDATE_GATE_SCHEMA_VERSION,
        phase: request.phase,
        disposition,
        base_commit: identity.effective_base_commit,
        candidate_commit: request.candidate_commit.to_string(),
        tester_results_path: request
            .tester_results_path
            .map(|path| evidence_path(request.project_root, path)),
        metadata_rewrite: identity.metadata_rewrite,
        checks,
    };
    let parent = request
        .artifact_path
        .parent()
        .context("candidate gate artifact path has no parent")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "create candidate gate artifact directory {}",
            parent.display()
        )
    })?;
    let serialized = serde_json::to_vec_pretty(&evidence)?;
    crate::atomic_write::atomic_write(request.artifact_path, &serialized).with_context(|| {
        format!(
            "write candidate gate evidence {}",
            request.artifact_path.display()
        )
    })?;
    Ok(evidence)
}

struct CandidateIdentityResolution {
    check: CandidateCheckEvidence,
    effective_base_commit: String,
    metadata_rewrite: Option<CandidateMetadataRewriteEvidence>,
}

fn check_candidate_identity(
    request: &CandidateGateRequest<'_>,
    prior_candidate_gate_path: Option<&Path>,
    writer_role: Option<&str>,
) -> Result<CandidateIdentityResolution> {
    let head = crate::git::run_stdout(
        request.workspace,
        &["rev-parse", "HEAD"],
        "resolve candidate gate HEAD",
    );
    let mut diagnostics = Vec::new();
    let mut effective_base_commit = request.base_commit.to_string();
    let mut metadata_rewrite = None;

    match head {
        Ok(head) if head == request.candidate_commit => {}
        Ok(head) => diagnostics.push(format!(
            "candidate workspace HEAD {head} does not match {}",
            request.candidate_commit
        )),
        Err(error) => diagnostics.push(format!("candidate HEAD cannot be resolved: {error:#}")),
    }

    if diagnostics.is_empty() {
        let worktree = crate::git::run_stdout(
            request.workspace,
            &["status", "--porcelain", "--untracked-files=all"],
            "verify candidate gate cleanliness",
        )?;
        if !worktree.is_empty() {
            diagnostics.push(format!(
                "candidate workspace is dirty before admission:\n{worktree}"
            ));
        }
    }

    if diagnostics.is_empty() {
        let metadata_recovery = writer_role == Some(CANDIDATE_METADATA_REPAIR_ROLE);
        match prior_candidate_gate_path {
            Some(path) => match read_prior_writer_gate(path) {
                Ok(prior) => {
                    if let Some(authorization) = request.metadata_rewrite_authorization {
                        // A review-derived rewrite starts from a passing gate.
                        // The older rejected-gate validator is intentionally
                        // strict for ordinary corrections, so use a separate
                        // passing-gate check for this explicitly audited path.
                        if let Err(reason) = validate_passing_writer_gate_for_metadata_rewrite(
                            &prior,
                            request.base_commit,
                        ) {
                            diagnostics.push(reason);
                        } else if !is_ancestor(
                            request.workspace,
                            &prior.base_commit,
                            &prior.candidate_commit,
                        )? {
                            diagnostics.push(format!(
                                "prior candidate {} does not descend from original base {}",
                                prior.candidate_commit, prior.base_commit
                            ));
                        } else {
                            match validate_authorized_metadata_rewrite(request, authorization)? {
                                Ok((base, evidence)) => {
                                    effective_base_commit = base;
                                    metadata_rewrite = Some(evidence);
                                }
                                Err(reason) => diagnostics.push(reason),
                            }
                        }
                    } else if let Err(reason) =
                        validate_prior_writer_gate(&prior, request.base_commit)
                    {
                        diagnostics.push(reason);
                    } else if !is_ancestor(
                        request.workspace,
                        &prior.base_commit,
                        &prior.candidate_commit,
                    )? {
                        diagnostics.push(format!(
                            "prior candidate {} does not descend from original base {}",
                            prior.candidate_commit, prior.base_commit
                        ));
                    } else {
                        effective_base_commit = prior.base_commit.clone();
                        let current_descends_from_rejected = is_ancestor(
                            request.workspace,
                            request.base_commit,
                            request.candidate_commit,
                        )?;
                        if metadata_recovery || !current_descends_from_rejected {
                            match validate_metadata_only_rewrite(request, path, &prior)? {
                                Ok(evidence) => metadata_rewrite = Some(evidence),
                                Err(reason) => diagnostics.push(reason),
                            }
                        }
                    }
                }
                Err(reason) => diagnostics.push(reason),
            },
            None => {
                if metadata_recovery {
                    diagnostics
                        .push("metadata-recovery Writer has no prior candidate gate".to_string());
                } else if is_ancestor(
                    request.workspace,
                    request.base_commit,
                    request.candidate_commit,
                )? {
                    // The normal append-only Writer path needs no recovery.
                } else if let Some(authorization) = request.metadata_rewrite_authorization {
                    match validate_authorized_metadata_rewrite(request, authorization)? {
                        Ok((base, evidence)) => {
                            effective_base_commit = base;
                            metadata_rewrite = Some(evidence);
                        }
                        Err(reason) => diagnostics.push(reason),
                    }
                } else if request.allow_tree_identical_metadata_rewrite {
                    match validate_completion_repair_metadata_rewrite(request)? {
                        Ok((base, evidence)) => {
                            effective_base_commit = base;
                            metadata_rewrite = Some(evidence);
                        }
                        Err(reason) => diagnostics.push(reason),
                    }
                } else {
                    diagnostics.push(format!(
                        "base commit {} is not an ancestor of candidate {}",
                        request.base_commit, request.candidate_commit
                    ));
                }
            }
        }
    }

    if diagnostics.is_empty()
        && !is_ancestor(
            request.workspace,
            &effective_base_commit,
            request.candidate_commit,
        )?
    {
        diagnostics.push(format!(
            "original base commit {effective_base_commit} is not an ancestor of candidate {}",
            request.candidate_commit
        ));
    }

    let status = if diagnostics.is_empty() {
        CandidateCheckStatus::Passed
    } else {
        CandidateCheckStatus::Error
    };
    Ok(CandidateIdentityResolution {
        check: check("candidate-identity", status, diagnostics),
        effective_base_commit,
        metadata_rewrite,
    })
}

fn validate_authorized_metadata_rewrite(
    request: &CandidateGateRequest<'_>,
    authorization: &AuthorizedMetadataRewriteContext,
) -> Result<std::result::Result<(String, CandidateMetadataRewriteEvidence), String>> {
    if request.base_commit != authorization.source_candidate_commit {
        return Ok(Err(format!(
            "metadata recovery expected source candidate {}, Writer reported base {}",
            authorization.source_candidate_commit, request.base_commit
        )));
    }
    if !is_ancestor(
        request.workspace,
        &authorization.protected_base_commit,
        request.base_commit,
    )? {
        return Ok(Err(format!(
            "protected base {} is not an ancestor of source candidate {}",
            authorization.protected_base_commit, request.base_commit
        )));
    }
    if !is_ancestor(
        request.workspace,
        &authorization.protected_base_commit,
        request.candidate_commit,
    )? {
        return Ok(Err(format!(
            "protected base {} is not an ancestor of replacement candidate {}",
            authorization.protected_base_commit, request.candidate_commit
        )));
    }
    let source_tree = crate::git::run_stdout(
        request.workspace,
        &[
            "rev-parse",
            &format!("{}^{{tree}}", authorization.source_candidate_commit),
        ],
        "resolve metadata recovery source tree",
    )?;
    let replacement_tree = crate::git::run_stdout(
        request.workspace,
        &[
            "rev-parse",
            &format!("{}^{{tree}}", request.candidate_commit),
        ],
        "resolve metadata recovery replacement tree",
    )?;
    if source_tree != authorization.candidate_tree {
        return Ok(Err(format!(
            "metadata recovery source tree {} does not match its authorized tree {}",
            source_tree, authorization.candidate_tree
        )));
    }
    if replacement_tree != authorization.candidate_tree {
        return Ok(Err(format!(
            "metadata recovery replacement tree {} does not match authorized tree {}",
            replacement_tree, authorization.candidate_tree
        )));
    }
    if let Some(reason) = equivalent_message_only_suffix_mismatch(
        request.workspace,
        &authorization.protected_base_commit,
        &authorization.source_candidate_commit,
        request.candidate_commit,
    )? {
        return Ok(Err(reason));
    }
    Ok(Ok((
        authorization.protected_base_commit.clone(),
        CandidateMetadataRewriteEvidence {
            prior_gate_path: None,
            authorization: CandidateMetadataRewriteAuthorization::ReviewFinding,
            original_base_commit: authorization.protected_base_commit.clone(),
            rejected_candidate_commit: authorization.source_candidate_commit.clone(),
            replacement_candidate_commit: request.candidate_commit.to_string(),
            candidate_tree: replacement_tree,
        },
    )))
}

fn validate_completion_repair_metadata_rewrite(
    request: &CandidateGateRequest<'_>,
) -> Result<std::result::Result<(String, CandidateMetadataRewriteEvidence), String>> {
    let base_tree = crate::git::run_stdout(
        request.workspace,
        &["rev-parse", &format!("{}^{{tree}}", request.base_commit)],
        "resolve completion-repair base tree",
    );
    let candidate_tree = crate::git::run_stdout(
        request.workspace,
        &[
            "rev-parse",
            &format!("{}^{{tree}}", request.candidate_commit),
        ],
        "resolve completion-repair candidate tree",
    );
    let (base_tree, candidate_tree) = match (base_tree, candidate_tree) {
        (Ok(base), Ok(candidate)) => (base, candidate),
        (Err(error), _) => {
            return Ok(Err(format!(
                "completion-repair base tree cannot be resolved: {error:#}"
            )));
        }
        (_, Err(error)) => {
            return Ok(Err(format!(
                "completion-repair candidate tree cannot be resolved: {error:#}"
            )));
        }
    };
    if base_tree != candidate_tree {
        return Ok(Err(format!(
            "completion-repair candidate tree {candidate_tree} does not match base tree {base_tree}"
        )));
    }
    let original_base = crate::git::run_stdout(
        request.workspace,
        &["merge-base", request.base_commit, request.candidate_commit],
        "resolve completion-repair original base",
    )?;
    if original_base.is_empty() {
        return Ok(Err(
            "completion-repair metadata rewrite has no common base".to_string()
        ));
    }
    Ok(Ok((
        original_base.clone(),
        CandidateMetadataRewriteEvidence {
            prior_gate_path: None,
            authorization: CandidateMetadataRewriteAuthorization::WriterCompletionRepair,
            original_base_commit: original_base,
            rejected_candidate_commit: request.base_commit.to_string(),
            replacement_candidate_commit: request.candidate_commit.to_string(),
            candidate_tree,
        },
    )))
}

fn is_ancestor(workspace: &Path, base: &str, candidate: &str) -> Result<bool> {
    Ok(
        crate::git::run_raw(workspace, &["merge-base", "--is-ancestor", base, candidate])?
            .status
            .success(),
    )
}

pub(crate) fn read_prior_writer_gate(
    path: &Path,
) -> std::result::Result<CandidateGateEvidence, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("prior candidate gate cannot be inspected: {error}"))?;
    if metadata.len() > CANDIDATE_GATE_MAX_BYTES {
        return Err(format!(
            "prior candidate gate is {} bytes; maximum is {CANDIDATE_GATE_MAX_BYTES} bytes",
            metadata.len()
        ));
    }
    let bytes =
        fs::read(path).map_err(|error| format!("prior candidate gate cannot be read: {error}"))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("prior candidate gate is malformed: {error}"))
}

pub(crate) fn validate_prior_writer_gate(
    prior: &CandidateGateEvidence,
    expected_candidate: &str,
) -> std::result::Result<(), String> {
    if prior.schema_version == 0 || prior.schema_version > CANDIDATE_GATE_SCHEMA_VERSION {
        return Err(format!(
            "prior candidate gate uses unsupported schema version {}",
            prior.schema_version
        ));
    }
    if !matches!(
        prior.phase,
        CandidateGatePhase::Writer | CandidateGatePhase::FinalTester
    ) || prior.disposition != CandidateGateDisposition::Rejected
    {
        return Err("prior candidate gate is not a rejected correction gate".to_string());
    }
    if prior.candidate_commit != expected_candidate {
        return Err(format!(
            "prior candidate gate names candidate {}, expected {expected_candidate}",
            prior.candidate_commit
        ));
    }
    let expected_checks = [
        "candidate-identity",
        "whitespace",
        "commit-metadata",
        "behavior-references",
        "tester-reference-evidence",
        "project-check",
    ];
    for name in expected_checks {
        if prior
            .checks
            .iter()
            .filter(|check| check.name == name)
            .count()
            != 1
        {
            return Err(format!(
                "prior candidate gate must contain exactly one {name} check"
            ));
        }
    }
    if prior.checks.len() != expected_checks.len() {
        return Err("prior candidate gate contains unknown or duplicate checks".to_string());
    }
    if prior
        .checks
        .iter()
        .any(|check| check.status == CandidateCheckStatus::Error)
    {
        return Err("prior candidate gate contains an untrustworthy check error".to_string());
    }
    if prior
        .checks
        .iter()
        .find(|check| check.name == "candidate-identity")
        .is_none_or(|check| check.status != CandidateCheckStatus::Passed)
    {
        return Err("prior candidate gate did not establish candidate identity".to_string());
    }
    Ok(())
}

/// Reclassify only the exact legacy project-check output that Factory's
/// checked-in policy now treats as presentation advice.
pub(crate) fn classify_retired_commit_style_gate(
    project_root: &Path,
    workspace: &Path,
    gate_path: &Path,
    expected_base: &str,
    expected_candidate: &str,
) -> Result<RetiredCommitStyleGate> {
    let gate_bytes = read_bounded_regular_file(
        gate_path,
        CANDIDATE_GATE_MAX_BYTES,
        "retired commit-style candidate gate",
    )?;
    let gate: CandidateGateEvidence =
        serde_json::from_slice(&gate_bytes).context("parse retired commit-style candidate gate")?;
    validate_prior_writer_gate(&gate, expected_candidate)
        .map_err(|reason| anyhow::anyhow!(reason))?;
    if gate.phase != CandidateGatePhase::Writer
        || gate.base_commit != expected_base
        || gate.tester_results_path.is_some()
        || gate.metadata_rewrite.is_some()
    {
        bail!("retired commit-style gate does not bind the exact source Writer candidate");
    }
    let failed = gate
        .checks
        .iter()
        .filter(|check| check.status == CandidateCheckStatus::Failed)
        .collect::<Vec<_>>();
    if failed.len() != 1 || failed[0].name != "project-check" {
        bail!("retired commit-style gate must fail only the project check");
    }
    for required in ["candidate-identity", "whitespace", "commit-metadata"] {
        if gate
            .checks
            .iter()
            .find(|check| check.name == required)
            .is_none_or(|check| check.status != CandidateCheckStatus::Passed)
        {
            bail!("retired commit-style gate did not pass {required}");
        }
    }
    let project_check = failed[0];
    let exit_codes = project_check
        .diagnostics
        .iter()
        .filter_map(|diagnostic| project_check_exit_code(diagnostic))
        .collect::<Vec<_>>();
    if project_check.diagnostics.len() != 1 || exit_codes.as_slice() != [1] {
        bail!("retired commit-style project check must record the canonical hook exit");
    }

    let expected_log_path = gate_path
        .parent()
        .context("retired commit-style gate path has no parent")?
        .join("hooks/check-pre-merge.log");
    let expected_log_relative = expected_log_path
        .strip_prefix(project_root)
        .map_err(|_| anyhow::anyhow!("retired commit-style gate is outside the project"))?
        .to_string_lossy()
        .to_string();
    if project_check.log_path.as_deref() != Some(expected_log_relative.as_str()) {
        bail!(
            "retired commit-style project check must bind the canonical hook log {expected_log_relative}"
        );
    }
    let log_bytes = read_bounded_regular_file(
        &expected_log_path,
        MAX_PROJECT_CHECK_LOG_BYTES,
        "retired commit-style hook log",
    )?;
    let log = String::from_utf8(log_bytes.clone())
        .map_err(|_| anyhow::anyhow!("retired commit-style hook log is not valid UTF-8"))?;
    let findings = log
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if findings.is_empty() {
        bail!("retired commit-style hook log has no findings");
    }

    let commits = crate::git::run_stdout(
        workspace,
        &[
            "rev-list",
            "--reverse",
            &format!("{expected_base}..{expected_candidate}"),
        ],
        "enumerate retired commit-style candidate commits",
    )?
    .lines()
    .map(str::to_string)
    .collect::<Vec<_>>();
    if commits.is_empty() {
        bail!("retired commit-style candidate has no commits after its base");
    }
    // The candidate's own historical checker may have omitted a still-active
    // project blocker, so the stored presentation log is not sufficient proof
    // of attribution safety.
    for commit in &commits {
        let message = crate::git::run_stdout(
            workspace,
            &["show", "-s", "--format=%B", commit],
            "inspect retired commit-style attribution",
        )?;
        if message.lines().any(|line| {
            line.trim_start()
                .to_ascii_lowercase()
                .starts_with("co-authored-by:")
        }) {
            bail!(
                "retired commit-style candidate {commit} contains a prohibited Co-Authored-By trailer"
            );
        }
    }
    for finding in &findings {
        validate_retired_commit_style_finding(workspace, finding, &commits)?;
    }

    let policy_commit = crate::git::run_stdout(
        project_root,
        &["rev-parse", "HEAD"],
        "resolve commit-message presentation policy",
    )?;
    validate_retired_commit_style_policy(project_root, &policy_commit)?;

    let candidate_tree = crate::git::run_stdout(
        workspace,
        &["rev-parse", &format!("{expected_candidate}^{{tree}}")],
        "resolve retired commit-style candidate tree",
    )?;
    Ok(RetiredCommitStyleGate {
        candidate_tree,
        gate_digest: sha256_digest(&gate_bytes),
        hook_log_path: expected_log_relative,
        hook_log_digest: sha256_digest(&log_bytes),
        findings,
        policy_commit,
    })
}

fn validate_retired_commit_style_policy(project_root: &Path, policy_commit: &str) -> Result<()> {
    let resolved = crate::git::run_stdout(
        project_root,
        &[
            "rev-parse",
            "--verify",
            &format!("{policy_commit}^{{commit}}"),
        ],
        "resolve checked-in commit-message presentation policy",
    )?;
    if resolved != policy_commit {
        bail!("retired commit-style policy must name one exact commit");
    }
    let policy_script = crate::git::run_stdout(
        project_root,
        &[
            "show",
            &format!("{policy_commit}:scripts/check-commit-message-policy.sh"),
        ],
        "read checked-in commit-message presentation policy",
    )?;
    if !policy_script
        .lines()
        .any(|line| line == COMMIT_PRESENTATION_ADVISORY_MARKER)
    {
        bail!("the checked-in commit-message policy has not retired presentation findings");
    }
    let project_hook = crate::git::run_stdout(
        project_root,
        &[
            "show",
            &format!("{policy_commit}:.fluent/hooks/check-pre-merge"),
        ],
        "read checked-in project candidate hook",
    )?;
    if !project_hook.lines().any(|line| {
        let line = line.trim();
        !line.starts_with('#') && line == "scripts/check-commit-message-policy.sh"
    }) {
        bail!("the checked-in project hook does not invoke Factory's commit-message policy");
    }

    Ok(())
}

/// Keep the receipt's original policy commit as immutable audit evidence while
/// allowing ordinary project commits to advance the still-active policy.
pub(crate) fn validate_retired_commit_style_policy_replay(
    project_root: &Path,
    recorded_policy_commit: &str,
    current_policy_commit: &str,
) -> Result<()> {
    validate_retired_commit_style_policy(project_root, recorded_policy_commit)?;
    if !is_ancestor(project_root, recorded_policy_commit, current_policy_commit)? {
        bail!("retired commit-style policy commit is not an ancestor of the current policy");
    }
    Ok(())
}

fn read_bounded_regular_file(path: &Path, maximum: u64, subject: &str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {subject} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{subject} is not a regular file: {}", path.display());
    }
    if metadata.len() > maximum {
        bail!(
            "{subject} is {} bytes; maximum is {maximum} bytes",
            metadata.len()
        );
    }
    fs::read(path).with_context(|| format!("read {subject} {}", path.display()))
}

fn validate_retired_commit_style_finding(
    workspace: &Path,
    diagnostic: &str,
    known_commits: &[String],
) -> Result<()> {
    let normalized = normalize_diagnostic_text(diagnostic);
    let rest = normalized
        .strip_prefix("commit ")
        .ok_or_else(|| anyhow::anyhow!("unknown retired commit-style hook diagnostic"))?;
    let (raw_commit, reason) = rest
        .split_once(' ')
        .ok_or_else(|| anyhow::anyhow!("malformed retired commit-style hook diagnostic"))?;
    let commit = resolve_commit_id(raw_commit.trim_end_matches(':'), known_commits)
        .ok_or_else(|| anyhow::anyhow!("retired commit-style finding has an unknown commit"))?;
    let message = crate::git::run_stdout(
        workspace,
        &["show", "-s", "--format=%B", &commit],
        "read retired commit-style message",
    )?;
    let lines = message.split('\n').collect::<Vec<_>>();
    if reason == "has no body separated from its subject by a blank line" {
        let has_body = lines.len() >= 3
            && lines[1].trim().is_empty()
            && lines[2..].iter().any(|line| !line.trim().is_empty());
        if has_body {
            bail!("retired missing-body finding does not match commit {commit}");
        }
        return Ok(());
    }
    let line_number = reason
        .strip_prefix("body line ")
        .and_then(|value| value.strip_suffix(" exceeds 72 characters"))
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|line| *line >= 2)
        .ok_or_else(|| anyhow::anyhow!("unknown retired commit-style hook diagnostic"))?;
    let line = lines
        .get(line_number - 1)
        .ok_or_else(|| anyhow::anyhow!("retired line-length finding names a missing line"))?;
    if line.chars().count() <= 72 {
        bail!("retired line-length finding does not match commit {commit}");
    }
    Ok(())
}

fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Validate the passing source gate used by an explicitly authorized review
/// finding. This is deliberately separate from `validate_prior_writer_gate`,
/// which admits only rejected correction gates for the ordinary append-only
/// recovery path.
fn validate_passing_writer_gate_for_metadata_rewrite(
    prior: &CandidateGateEvidence,
    expected_candidate: &str,
) -> std::result::Result<(), String> {
    if prior.schema_version == 0 || prior.schema_version > CANDIDATE_GATE_SCHEMA_VERSION {
        return Err(format!(
            "source candidate gate uses unsupported schema version {}",
            prior.schema_version
        ));
    }
    if prior.phase != CandidateGatePhase::Writer
        || prior.disposition != CandidateGateDisposition::Passed
    {
        return Err("source candidate gate is not a passing Writer gate".to_string());
    }
    if prior.candidate_commit != expected_candidate {
        return Err(format!(
            "source candidate gate names candidate {}, expected {expected_candidate}",
            prior.candidate_commit
        ));
    }
    let expected_checks = [
        "candidate-identity",
        "whitespace",
        "commit-metadata",
        "behavior-references",
        "tester-reference-evidence",
        "project-check",
    ];
    if prior.checks.len() != expected_checks.len()
        || expected_checks.iter().any(|name| {
            prior
                .checks
                .iter()
                .filter(|check| check.name == *name)
                .count()
                != 1
        })
    {
        return Err("source candidate gate has an incomplete or duplicate check set".to_string());
    }
    if prior.checks.iter().any(|check| {
        matches!(
            check.status,
            CandidateCheckStatus::Failed | CandidateCheckStatus::Error
        )
    }) {
        return Err("source candidate gate contains a failed or untrustworthy check".to_string());
    }
    if prior
        .checks
        .iter()
        .find(|check| check.name == "candidate-identity")
        .is_none_or(|check| check.status != CandidateCheckStatus::Passed)
    {
        return Err("source candidate gate did not establish candidate identity".to_string());
    }
    Ok(())
}

/// Return the protected base when one rejected metadata gate authorizes an
/// exact tree-identical rewrite of `expected_candidate`.
pub(crate) fn authorized_tree_identical_metadata_rewrite_base(
    path: &Path,
    expected_candidate: &str,
) -> Option<String> {
    let prior = read_prior_writer_gate(path).ok()?;
    validate_prior_writer_gate(&prior, expected_candidate).ok()?;
    prior
        .checks
        .iter()
        .filter(|check| check.status == CandidateCheckStatus::Failed)
        .map(|check| check.name.as_str())
        .eq(["commit-metadata"])
        .then_some(prior.base_commit)
}

/// Recognize only the blocked gate shape produced when an otherwise valid,
/// tree-identical metadata repair replaced its protected base commit.
pub(crate) fn candidate_ancestry_repair_authorization(
    evidence: &CandidateGateEvidence,
) -> Option<CandidateAncestryRepairAuthorization> {
    if evidence.schema_version == 0
        || evidence.schema_version > CANDIDATE_GATE_SCHEMA_VERSION
        || evidence.phase != CandidateGatePhase::Writer
        || evidence.disposition != CandidateGateDisposition::Blocked
    {
        return None;
    }
    let rewrite = evidence.metadata_rewrite.as_ref()?;
    if rewrite.authorization != CandidateMetadataRewriteAuthorization::RejectedMetadataGate
        || evidence.base_commit != rewrite.original_base_commit
        || evidence.candidate_commit != rewrite.replacement_candidate_commit
    {
        return None;
    }
    let prior_gate_path = rewrite.prior_gate_path.clone()?;
    let expected_checks = [
        "candidate-identity",
        "whitespace",
        "commit-metadata",
        "behavior-references",
        "tester-reference-evidence",
        "project-check",
    ];
    if evidence.checks.len() != expected_checks.len()
        || expected_checks.iter().any(|name| {
            evidence
                .checks
                .iter()
                .filter(|check| check.name == *name)
                .count()
                != 1
        })
    {
        return None;
    }
    let expected_diagnostic = format!(
        "original base commit {} is not an ancestor of candidate {}",
        rewrite.original_base_commit, evidence.candidate_commit
    );
    let mut blocking_checks = evidence.checks.iter().filter(|check| {
        matches!(
            check.status,
            CandidateCheckStatus::Failed | CandidateCheckStatus::Error
        )
    });
    let identity = blocking_checks.next()?;
    if identity.name != "candidate-identity"
        || identity.status != CandidateCheckStatus::Error
        || identity.diagnostics.as_slice() != [expected_diagnostic]
        || blocking_checks.next().is_some()
    {
        return None;
    }
    Some(CandidateAncestryRepairAuthorization {
        protected_base_commit: rewrite.original_base_commit.clone(),
        rejected_candidate_commit: rewrite.rejected_candidate_commit.clone(),
        candidate_commit: evidence.candidate_commit.clone(),
        candidate_tree: rewrite.candidate_tree.clone(),
        prior_gate_path,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataFinding {
    commit: String,
    reason: String,
}

/// Validate a project check that derives from the preceding gate's commit
/// metadata finding.  The gate owns this classifier so both the recovery
/// command and the candidate gate apply the same evidence contract.
pub(crate) fn validate_derived_project_check(
    project_root: &Path,
    gate_path: &Path,
    metadata_check: &CandidateCheckEvidence,
    project_check: &CandidateCheckEvidence,
) -> Result<String> {
    if metadata_check.status != CandidateCheckStatus::Failed
        || project_check.status != CandidateCheckStatus::Failed
    {
        bail!("derived project-check evidence must contain two failed checks");
    }
    let gate_findings = metadata_findings(project_root, &metadata_check.diagnostics, &[], false)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "project-check failure is not explained by structured commit-metadata diagnostics"
            )
        })?;
    if gate_findings.is_empty() {
        bail!("project-check failure is not explained by structured commit-metadata diagnostics");
    }
    let known_commits = gate_findings
        .iter()
        .map(|finding| finding.commit.clone())
        .fold(Vec::new(), |mut commits, commit| {
            if !commits.contains(&commit) {
                commits.push(commit);
            }
            commits
        });

    let expected_log_path = gate_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("candidate gate artifact path has no parent"))?
        .join("hooks/check-pre-merge.log");
    let expected_log_rel = expected_log_path
        .strip_prefix(project_root)
        .map_err(|_| anyhow::anyhow!("candidate gate artifact path is outside the project"))?
        .to_string_lossy()
        .to_string();
    if project_check.log_path.as_deref() != Some(expected_log_rel.as_str()) {
        bail!("derived project-check hook log must be the canonical {expected_log_rel} artifact");
    }

    let metadata = fs::symlink_metadata(&expected_log_path).with_context(|| {
        format!(
            "inspect derived project-check hook log {}",
            expected_log_path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "derived project-check hook log is not a regular file: {}",
            expected_log_path.display()
        );
    }
    if metadata.len() > MAX_PROJECT_CHECK_LOG_BYTES {
        bail!(
            "derived project-check hook log is {} bytes; maximum is {MAX_PROJECT_CHECK_LOG_BYTES} bytes",
            metadata.len()
        );
    }

    let exit_codes = project_check
        .diagnostics
        .iter()
        .filter_map(|diagnostic| project_check_exit_code(diagnostic))
        .collect::<Vec<_>>();
    if exit_codes.len() != 1 || exit_codes[0] == 0 {
        bail!(
            "derived project-check evidence must contain exactly one nonzero check-pre-merge exit"
        );
    }
    if project_check
        .diagnostics
        .iter()
        .any(|diagnostic| project_check_exit_code(diagnostic).is_none())
    {
        bail!("derived project-check diagnostics must be represented in the stored hook log");
    }

    let log = String::from_utf8(fs::read(&expected_log_path).with_context(|| {
        format!(
            "read derived project-check hook log {}",
            expected_log_path.display()
        )
    })?)
    .map_err(|_| anyhow::anyhow!("derived project-check hook log is not valid UTF-8"))?;
    let log_diagnostics = log
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if log_diagnostics.is_empty() {
        bail!("derived project-check hook log has no structured hook diagnostics");
    }
    let log_findings = metadata_findings(project_root, &log_diagnostics, &known_commits, true)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "project-check hook log contains a diagnostic unrelated to commit-metadata findings"
            )
        })?;
    if log_findings.is_empty() {
        bail!("derived project-check hook log has no structured hook diagnostics");
    }
    if !log_diagnostics.iter().any(|diagnostic| {
        parse_metadata_finding(diagnostic, &known_commits, true).is_some_and(
            |(finding, raw_reason)| {
                is_body_line_length_reason(&raw_reason)
                    && gate_findings.iter().any(|gate| {
                        gate.commit == finding.commit && gate.reason == "literal-line-breaks"
                    })
            },
        )
    }) {
        bail!(
            "derived project-check hook log does not contain the required body-line-length finding"
        );
    }

    let diagnostic_findings = project_check
        .diagnostics
        .iter()
        .filter(|diagnostic| !is_project_check_exit_diagnostic(diagnostic))
        .cloned()
        .collect::<Vec<_>>();
    let diagnostic_findings = metadata_findings(
        project_root,
        &diagnostic_findings,
        &known_commits,
        true,
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
            "project-check diagnostics contain a diagnostic unrelated to commit-metadata findings"
        )
    })?;
    if diagnostic_findings
        .iter()
        .any(|finding| !log_findings.contains(finding))
    {
        bail!("project-check diagnostics are not represented in the stored hook log");
    }

    let mut remaining = gate_findings;
    for finding in log_findings {
        let Some(index) = remaining.iter().position(|candidate| candidate == &finding) else {
            bail!(
                "project-check hook log contains a diagnostic unrelated to commit-metadata findings"
            );
        };
        remaining.remove(index);
    }
    Ok(expected_log_rel)
}

fn metadata_findings(
    project_root: &Path,
    diagnostics: &[String],
    known_commits: &[String],
    allow_project_policy_wording: bool,
) -> Option<Vec<MetadataFinding>> {
    diagnostics
        .iter()
        .map(|diagnostic| {
            let (finding, raw_reason) =
                parse_metadata_finding(diagnostic, known_commits, allow_project_policy_wording)?;
            if let Some(line_number) = project_policy_line_number(&raw_reason)
                && !commit_message_line_proves_literal_breaks(
                    project_root,
                    &finding.commit,
                    line_number,
                )?
            {
                return None;
            }
            Some(finding)
        })
        .collect()
}

fn parse_metadata_finding(
    diagnostic: &str,
    known_commits: &[String],
    allow_project_policy_wording: bool,
) -> Option<(MetadataFinding, String)> {
    let normalized = normalize_diagnostic_text(diagnostic);
    let rest = normalized.strip_prefix("commit ")?;
    let (raw_commit, reason) = rest.split_once(' ')?;
    let raw_commit = raw_commit.trim_end_matches(':');
    let commit = resolve_commit_id(raw_commit, known_commits)?;
    let raw_reason = reason.to_ascii_lowercase();
    let normalized_reason = normalize_metadata_reason(&raw_reason, allow_project_policy_wording)?;
    Some((
        MetadataFinding {
            commit,
            reason: normalized_reason,
        },
        raw_reason,
    ))
}

fn normalize_diagnostic_text(diagnostic: &str) -> String {
    diagnostic.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn resolve_commit_id(raw_commit: &str, known_commits: &[String]) -> Option<String> {
    if !(4..=64).contains(&raw_commit.len())
        || !raw_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let raw_commit = raw_commit.to_ascii_lowercase();
    if known_commits.is_empty() {
        return Some(raw_commit);
    }
    let mut matches = known_commits
        .iter()
        .filter(|commit| commit.as_str() == raw_commit || commit.starts_with(&raw_commit))
        .map(String::as_str)
        .collect::<Vec<_>>();
    matches.sort_unstable();
    matches.dedup();
    (matches.len() == 1).then(|| matches[0].to_string())
}

fn normalize_metadata_reason(reason: &str, allow_project_policy_wording: bool) -> Option<String> {
    if allow_project_policy_wording && is_body_line_length_reason(reason) {
        return Some("literal-line-breaks".to_string());
    }
    match reason {
        "is a merge commit" => Some("merge-commit".to_string()),
        "has an empty subject" => Some("empty-subject".to_string()),
        "contains literal \\n text instead of line breaks" => {
            Some("literal-line-breaks".to_string())
        }
        _ => reason.strip_prefix("has invalid ").and_then(|label| {
            let label = label.split(':').next()?.trim();
            matches!(
                label,
                "author name" | "author email" | "committer name" | "committer email"
            )
            .then(|| format!("invalid-{}", label.replace(' ', "-")))
        }),
    }
}

fn is_body_line_length_reason(reason: &str) -> bool {
    let mut fields = reason.split_whitespace();
    if fields.next() != Some("body") || fields.next() != Some("line") {
        return false;
    }
    let Some(line) = fields.next() else {
        return false;
    };
    let Ok(line_number) = line.parse::<u64>() else {
        return false;
    };
    if line.is_empty() || !line.bytes().all(|byte| byte.is_ascii_digit()) || line_number == 0 {
        return false;
    }
    matches!(
        (fields.next(), fields.next(), fields.next(), fields.next()),
        (Some("exceeds"), Some("72"), Some("characters"), None)
    )
}

fn project_policy_line_number(reason: &str) -> Option<usize> {
    let reason = reason.to_ascii_lowercase();
    let line_number = reason
        .strip_prefix("body line ")
        .and_then(|line| line.strip_suffix(" exceeds 72 characters"))?;
    (!line_number.is_empty() && line_number.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| line_number.parse::<usize>().ok())
        .flatten()
        .filter(|line| *line > 0)
}

fn commit_message_line_proves_literal_breaks(
    project_root: &Path,
    commit: &str,
    line_number: usize,
) -> Option<bool> {
    let output = crate::git::run_raw(project_root, &["show", "-s", "--format=%B", commit]).ok()?;
    if !output.status.success() {
        return None;
    }
    let message = String::from_utf8(output.stdout).ok()?;
    let line = message.split('\n').nth(line_number.checked_sub(1)?)?;
    Some(line.chars().count() > 72 && line.contains("\\n"))
}

fn is_project_check_exit_diagnostic(diagnostic: &str) -> bool {
    project_check_exit_code(diagnostic).is_some()
}

fn project_check_exit_code(diagnostic: &str) -> Option<i32> {
    let normalized = normalize_diagnostic_text(diagnostic);
    normalized
        .strip_prefix("check-pre-merge exited ")
        .and_then(|code| code.parse::<i32>().ok())
}

fn validate_metadata_only_rewrite(
    request: &CandidateGateRequest<'_>,
    prior_gate_path: &Path,
    prior: &CandidateGateEvidence,
) -> Result<std::result::Result<CandidateMetadataRewriteEvidence, String>> {
    let failed = prior
        .checks
        .iter()
        .filter(|check| check.status == CandidateCheckStatus::Failed)
        .map(|check| check.name.as_str())
        .collect::<Vec<_>>();
    if failed == ["commit-metadata", "project-check"] {
        let metadata_check = prior
            .checks
            .iter()
            .find(|check| check.name == "commit-metadata")
            .ok_or_else(|| anyhow::anyhow!("prior gate has no commit-metadata check"))?;
        let project_check = prior
            .checks
            .iter()
            .find(|check| check.name == "project-check")
            .ok_or_else(|| anyhow::anyhow!("prior gate has no project-check check"))?;
        if let Err(error) = validate_derived_project_check(
            request.project_root,
            prior_gate_path,
            metadata_check,
            project_check,
        ) {
            return Ok(Err(format!(
                "candidate history changed after prior gate failures: {error:#}"
            )));
        }
    } else if failed != ["commit-metadata"] {
        return Ok(Err(format!(
            "candidate history changed after prior gate failures: {}",
            if failed.is_empty() {
                "none".to_string()
            } else {
                failed.join(", ")
            }
        )));
    }
    let rejected_tree = crate::git::run_stdout(
        request.workspace,
        &["rev-parse", &format!("{}^{{tree}}", prior.candidate_commit)],
        "resolve rejected candidate tree",
    );
    let replacement_tree = crate::git::run_stdout(
        request.workspace,
        &[
            "rev-parse",
            &format!("{}^{{tree}}", request.candidate_commit),
        ],
        "resolve replacement candidate tree",
    );
    let (rejected_tree, replacement_tree) = match (rejected_tree, replacement_tree) {
        (Ok(rejected), Ok(replacement)) => (rejected, replacement),
        (Err(error), _) => {
            return Ok(Err(format!(
                "rejected candidate tree cannot be resolved: {error:#}"
            )));
        }
        (_, Err(error)) => {
            return Ok(Err(format!(
                "replacement candidate tree cannot be resolved: {error:#}"
            )));
        }
    };
    if rejected_tree != replacement_tree {
        return Ok(Err(format!(
            "rewritten candidate tree {replacement_tree} does not match rejected tree {rejected_tree}"
        )));
    }
    // A replacement that still descends from the protected base must preserve
    // the exact linear, per-commit tree/author suffix.  If it replaced that
    // base, retain the metadata-rewrite evidence so the dedicated ancestry
    // recovery can classify and repair the protected-base failure.
    if is_ancestor(
        request.workspace,
        &prior.base_commit,
        request.candidate_commit,
    )? && let Some(reason) = equivalent_message_only_suffix_mismatch(
        request.workspace,
        &prior.base_commit,
        &prior.candidate_commit,
        request.candidate_commit,
    )? {
        return Ok(Err(reason));
    }
    Ok(Ok(CandidateMetadataRewriteEvidence {
        prior_gate_path: Some(evidence_path(request.project_root, prior_gate_path)),
        authorization: CandidateMetadataRewriteAuthorization::RejectedMetadataGate,
        original_base_commit: prior.base_commit.clone(),
        rejected_candidate_commit: prior.candidate_commit.clone(),
        replacement_candidate_commit: request.candidate_commit.to_string(),
        candidate_tree: replacement_tree,
    }))
}

/// Return a diagnostic when a candidate rewrite changes anything beyond commit
/// messages.  The rejected and replacement tips must expose the same linear
/// suffix from the protected base, with identical trees and author identity at
/// each position.  Committer timestamps may change as a consequence of the
/// authorized rewrite; the candidate metadata gate still validates the new
/// committer identity and message.
fn equivalent_message_only_suffix_mismatch(
    workspace: &Path,
    protected_base: &str,
    rejected_candidate: &str,
    replacement_candidate: &str,
) -> Result<Option<String>> {
    let rejected = linear_commit_suffix(workspace, protected_base, rejected_candidate)?;
    let replacement = linear_commit_suffix(workspace, protected_base, replacement_candidate)?;
    if rejected.is_empty() || replacement.is_empty() {
        return Ok(Some(
            "metadata rewrite candidates must each contain a non-empty protected-base suffix"
                .to_string(),
        ));
    }
    if rejected.len() != replacement.len() {
        return Ok(Some(format!(
            "metadata rewrite changed the protected-base suffix length from {} to {}",
            rejected.len(),
            replacement.len()
        )));
    }
    for (old, new) in rejected.iter().zip(&replacement) {
        let old_identity = commit_author_identity(workspace, old)?;
        let new_identity = commit_author_identity(workspace, new)?;
        if old_identity != new_identity {
            return Ok(Some(format!(
                "metadata rewrite changed tree or author identity for commit {old}"
            )));
        }
    }
    Ok(None)
}

fn linear_commit_suffix(workspace: &Path, base: &str, tip: &str) -> Result<Vec<String>> {
    if !is_ancestor(workspace, base, tip)? {
        return Ok(Vec::new());
    }
    let raw = crate::git::run_stdout(
        workspace,
        &[
            "rev-list",
            "--reverse",
            "--first-parent",
            &format!("{base}..{tip}"),
        ],
        "enumerate metadata rewrite suffix",
    )?;
    let commits = raw.lines().map(str::to_string).collect::<Vec<_>>();
    let mut parent = base.to_string();
    for commit in &commits {
        let row = crate::git::run_stdout(
            workspace,
            &["rev-list", "--parents", "-n", "1", commit],
            "verify metadata rewrite linearity",
        )?;
        let fields = row.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2 || fields[0] != commit || fields[1] != parent {
            return Ok(Vec::new());
        }
        parent = commit.clone();
    }
    Ok(commits)
}

fn commit_author_identity(workspace: &Path, commit: &str) -> Result<Vec<String>> {
    let raw = crate::git::run_stdout(
        workspace,
        &["show", "-s", "--format=%T%x00%an%x00%ae%x00%aI", commit],
        "read metadata rewrite author identity",
    )?;
    Ok(raw.split('\0').map(str::to_string).collect())
}

fn check_whitespace(
    request: &CandidateGateRequest<'_>,
    base_commit: &str,
) -> Result<CandidateCheckEvidence> {
    let range = format!("{}..{}", base_commit, request.candidate_commit);
    let output = crate::git::run_raw(request.workspace, &["diff", "--check", &range])?;
    if output.status.success() {
        Ok(check(
            "whitespace",
            CandidateCheckStatus::Passed,
            Vec::new(),
        ))
    } else {
        Ok(check(
            "whitespace",
            CandidateCheckStatus::Failed,
            output_diagnostics(&output),
        ))
    }
}

fn check_commit_metadata(
    request: &CandidateGateRequest<'_>,
    base_commit: &str,
) -> Result<CandidateCheckEvidence> {
    let range = format!("{base_commit}..{}", request.candidate_commit);
    let output = crate::git::run_raw(
        request.workspace,
        &[
            "log",
            "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%cn%x1f%ce%x1f%B%x1e",
            &range,
        ],
    )?;
    if !output.status.success() {
        return Ok(check(
            "commit-metadata",
            CandidateCheckStatus::Error,
            output_diagnostics(&output),
        ));
    }
    let source = String::from_utf8_lossy(&output.stdout);
    let mut diagnostics = Vec::new();
    for record in source
        .split('\x1e')
        .filter(|record| !record.trim().is_empty())
    {
        let fields = record
            .trim_start_matches('\n')
            .splitn(7, '\x1f')
            .collect::<Vec<_>>();
        if fields.len() != 7 {
            diagnostics.push("Git returned malformed commit metadata".to_string());
            continue;
        }
        let commit = fields[0];
        let parents = fields[1].split_whitespace().count();
        if parents > 1 {
            diagnostics.push(format!("commit {commit} is a merge commit"));
        }
        for (label, value) in [
            ("author name", fields[2]),
            ("author email", fields[3]),
            ("committer name", fields[4]),
            ("committer email", fields[5]),
        ] {
            if value.trim().is_empty() || value.ends_with(".invalid") {
                diagnostics.push(format!("commit {commit} has invalid {label}: {value:?}"));
            }
        }
        let message = fields[6];
        if message
            .lines()
            .next()
            .is_none_or(|subject| subject.trim().is_empty())
        {
            diagnostics.push(format!("commit {commit} has an empty subject"));
        }
    }
    let status = if diagnostics.is_empty() {
        CandidateCheckStatus::Passed
    } else {
        CandidateCheckStatus::Failed
    };
    Ok(check("commit-metadata", status, diagnostics))
}

fn check_behavior_references(
    request: &CandidateGateRequest<'_>,
) -> Result<Vec<CandidateCheckEvidence>> {
    let Some(contract) = request.completion_contract else {
        return Ok(vec![
            check(
                "behavior-references",
                CandidateCheckStatus::Skipped,
                vec!["Attempt has no Writer completion contract".to_string()],
            ),
            check(
                "tester-reference-evidence",
                CandidateCheckStatus::Skipped,
                vec!["Attempt has no Writer completion contract".to_string()],
            ),
        ]);
    };
    let mut diagnostics = Vec::new();
    let mut test_references = Vec::new();
    for requirement in contract
        .requirements
        .iter()
        .filter(|row| row.kind == WriterCompletionRequirementKind::Behavior)
    {
        if requirement.verification_refs.is_empty() {
            diagnostics.push(format!(
                "{} has neither a Test: reference nor an Untestable: reason",
                requirement.source
            ));
            continue;
        }
        for reference in &requirement.verification_refs {
            if let Some(reason) = reference.strip_prefix("Untestable:") {
                if reason.trim().is_empty() {
                    diagnostics.push(format!(
                        "{} has an empty Untestable: reason",
                        requirement.source
                    ));
                }
                continue;
            }
            match parse_test_reference(reference) {
                Ok(test_reference) => {
                    let object = format!("{}:{}", request.candidate_commit, test_reference.path);
                    let output = crate::git::run_raw(request.workspace, &["show", &object])?;
                    if !output.status.success() {
                        diagnostics.push(format!(
                            "{} references missing committed test path {}",
                            requirement.source, test_reference.path
                        ));
                        continue;
                    }
                    if let Some(selector) = test_reference.selector.as_deref()
                        && !test_reference
                            .source_contains_selector(&String::from_utf8_lossy(&output.stdout))
                    {
                        diagnostics.push(format!(
                            "{} references test identifier {selector:?}, which is absent from {}",
                            requirement.source, test_reference.path
                        ));
                        continue;
                    }
                    test_references.push(test_reference);
                }
                Err(reason) => diagnostics.push(format!(
                    "{} has malformed Test: reference {reference:?}: {reason}",
                    requirement.source
                )),
            }
        }
    }
    let reference_status = if diagnostics.is_empty() {
        CandidateCheckStatus::Passed
    } else {
        CandidateCheckStatus::Failed
    };
    let reference_check = check("behavior-references", reference_status, diagnostics);
    let tester_check = check_tester_evidence(request, &test_references)?;
    Ok(vec![reference_check, tester_check])
}

#[derive(Debug)]
struct TestReference {
    path: String,
    selector: Option<String>,
    command: Option<String>,
}

impl TestReference {
    fn python_identity(&self) -> Option<PythonTestIdentity<'_>> {
        if !self.path.ends_with(".py") {
            return None;
        }
        let mut identity = PythonTestIdentity::parse_name(self.selector.as_deref()?)?;
        // Source selectors name a class and method in this file, not an import
        // alias whose module would require executing candidate Python to resolve.
        if identity.module.is_some() {
            return None;
        }
        identity.path = Some(&self.path);
        Some(identity)
    }

    fn rust_identity(&self) -> Option<RustTestIdentity<'_>> {
        if !self.path.ends_with(".rs") {
            return None;
        }
        let mut identity = RustTestIdentity::parse(self.selector.as_deref()?)?;
        if identity.path.is_some_and(|path| path != self.path) {
            return None;
        }
        identity.path = Some(&self.path);
        Some(identity)
    }

    fn source_contains_selector(&self, source: &str) -> bool {
        if self.path.ends_with(".sh") {
            // A shell script is one native test; its parenthetical describes
            // coverage and need not appear in the committed script.
            true
        } else if self.path.ends_with(".rs") {
            // Rust declares modules separately from the test function. The
            // final evidence check resolves its full native qualification.
            self.rust_identity()
                .is_some_and(|identity| source.contains(identity.short_name()))
        } else if self.path.ends_with(".py") && self.selector.is_some() {
            self.python_identity()
                .is_some_and(|identity| identity.is_defined_in(source))
        } else {
            self.selector
                .as_deref()
                .is_none_or(|selector| source.contains(selector))
        }
    }
}

#[derive(Debug)]
struct PythonTestIdentity<'a> {
    path: Option<&'a str>,
    module: Option<&'a str>,
    class: Option<&'a str>,
    method: &'a str,
}

impl<'a> PythonTestIdentity<'a> {
    fn parse_name(name: &'a str) -> Option<Self> {
        if !name.split('.').all(is_python_identifier) {
            return None;
        }
        let (qualification, method) = name
            .rsplit_once('.')
            .map_or((None, name), |(prefix, method)| (Some(prefix), method));
        let (module, class) = qualification.map_or((None, None), |qualification| {
            qualification
                .rsplit_once('.')
                .map_or((None, Some(qualification)), |(module, class)| {
                    (Some(module), Some(class))
                })
        });
        Some(Self {
            path: None,
            module,
            class,
            method,
        })
    }

    fn parse(identity: &'a str) -> Option<Self> {
        if let Some((prefix, wrapped)) = identity.split_once(" (") {
            let wrapped = wrapped.strip_suffix(')')?;
            if prefix.ends_with(".py") {
                validate_test_path(prefix).ok()?;
                let mut parsed = Self::parse_name(wrapped)?;
                parsed.path = Some(prefix);
                return Some(parsed);
            }
            // unittest displays either `method (module.Class)` or
            // `method (module.Class.method)`. Preserve the supplied class.
            if !is_python_identifier(prefix) {
                return None;
            }
            let mut parsed = Self::parse_name(wrapped)?;
            if parsed.method == prefix && parsed.class.is_some() {
                return Some(parsed);
            }
            let (module, class) = wrapped
                .rsplit_once('.')
                .map_or((None, wrapped), |(module, class)| (Some(module), class));
            parsed.module = module;
            parsed.class = Some(class);
            parsed.method = prefix;
            return Some(parsed);
        }
        if let Some((prefix, name)) = identity.split_once(".py::") {
            let path = &identity[..prefix.len() + 3];
            validate_test_path(path).ok()?;
            let mut parsed = Self::parse_name(name)?;
            parsed.path = Some(path);
            return Some(parsed);
        }
        Self::parse_name(identity)
    }

    fn compatible_with(&self, other: &Self) -> bool {
        self.method == other.method
            && !matches!((self.path, other.path), (Some(a), Some(b)) if a != b)
            && !matches!((self.module, other.module), (Some(a), Some(b)) if a != b)
            && !matches!((self.class, other.class), (Some(a), Some(b)) if a != b)
    }

    fn is_defined_in(&self, source: &str) -> bool {
        let Some(statements) = python_source_statements(source) else {
            return false;
        };
        let mut scopes: Vec<(usize, Option<&str>)> = Vec::new();
        let mut classes = 0;
        let mut methods = 0;
        for (indent, statement) in &statements {
            while scopes.last().is_some_and(|(parent, _)| parent >= indent) {
                scopes.pop();
            }
            let statement = statement.trim();
            let class = statement.strip_prefix("class ").and_then(|rest| {
                let end = rest.find(['(', ':'])?;
                let name = rest[..end].trim();
                is_python_identifier(name).then_some(name)
            });
            if scopes.is_empty() && class.is_some() && class == self.class {
                classes += 1;
            }
            let method = statement
                .strip_prefix("async ")
                .unwrap_or(statement)
                .strip_prefix("def ")
                .and_then(|rest| {
                    let (name, _) = rest.split_once('(')?;
                    let name = name.trim();
                    is_python_identifier(name).then_some(name)
                });
            let owner = scopes.last().and_then(|(_, class)| *class);
            if method == Some(self.method) && (self.class.is_none() || owner == self.class) {
                methods += 1;
            }
            if statement.ends_with(':') {
                // Only direct methods of a top-level class establish ownership.
                scopes.push((*indent, if scopes.is_empty() { class } else { None }));
            }
        }
        methods > 0 && (self.class.is_none() || (classes == 1 && methods == 1))
    }
}

fn is_python_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

// Scan logical statements without importing Python or evaluating its strings.
// Mask string contents and comments before using indentation to resolve scopes.
fn python_source_statements(source: &str) -> Option<Vec<(usize, String)>> {
    let mut statements = Vec::new();
    let mut current = Vec::new();
    let mut indent = 0;
    let mut quote: Option<(u8, bool)> = None;
    let mut brackets = Vec::new();
    for line in source.lines() {
        if current.is_empty() {
            indent = line
                .bytes()
                .take_while(|b| matches!(b, b' ' | b'\t'))
                .fold(0, |column, b| {
                    if b == b'\t' {
                        (column / 8 + 1) * 8
                    } else {
                        column + 1
                    }
                });
        }
        let bytes = line.as_bytes();
        let mut i = 0;
        let mut continued = false;
        while i < bytes.len() {
            let b = bytes[i];
            if let Some((delimiter, triple)) = quote {
                if b == b'\\' {
                    i += 2;
                    continue;
                }
                if b == delimiter && (!triple || bytes.get(i..i + 3) == Some(&[delimiter; 3])) {
                    quote = None;
                    i += if triple { 3 } else { 1 };
                } else {
                    i += 1;
                }
                continue;
            }
            match b {
                b'#' => break,
                b'\'' | b'"' => {
                    let triple = bytes.get(i..i + 3) == Some(&[b; 3]);
                    quote = Some((b, triple));
                    current.push(b'?');
                    i += if triple { 3 } else { 1 };
                    continue;
                }
                b'(' | b'[' | b'{' => brackets.push(b),
                b')' | b']' | b'}' => {
                    let opening = brackets.pop()?;
                    if !matches!((opening, b), (b'(', b')') | (b'[', b']') | (b'{', b'}')) {
                        return None;
                    }
                }
                b'\\' if i + 1 == bytes.len() => {
                    continued = true;
                    break;
                }
                _ => {}
            }
            current.push(b);
            i += 1;
        }
        if quote.is_none() && brackets.is_empty() && !continued {
            let statement = String::from_utf8(std::mem::take(&mut current)).ok()?;
            if !statement.trim().is_empty() {
                statements.push((indent, statement));
            }
        } else {
            current.push(b' ');
        }
    }
    (quote.is_none() && brackets.is_empty() && current.is_empty()).then_some(statements)
}

#[derive(Debug)]
struct RustTestIdentity<'a> {
    path: Option<&'a str>,
    binary: Option<&'a str>,
    name: &'a str,
}

impl<'a> RustTestIdentity<'a> {
    fn parse(identity: &'a str) -> Option<Self> {
        let (path, native) = if let Some((path, wrapped)) = identity.split_once(" (") {
            (Some(path), wrapped.strip_suffix(')')?)
        } else if let Some((path, native)) = identity.split_once(".rs::") {
            (Some(&identity[..path.len() + 3]), native)
        } else {
            (None, identity)
        };
        if let Some(path) = path
            && (!path.ends_with(".rs") || validate_test_path(path).is_err())
        {
            return None;
        }
        let (binary, name) = if let Some((binary, name)) = native.split_once('$') {
            (Some(binary), name)
        } else if let Some((binary, name)) = native.split_once(' ') {
            (Some(binary), name)
        } else {
            (None, native)
        };
        // Admit native names and the known nextest separators, never arbitrary
        // display prose or suffixes. Keep every supplied qualifier for matching.
        let valid_name = |name: &str| {
            name.split("::").all(|part| {
                let part = part.strip_prefix("r#").unwrap_or(part);
                let mut chars = part.chars();
                chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
                    && chars.all(|c| c == '_' || c.is_alphanumeric())
            })
        };
        if !valid_name(name) || binary.is_some_and(|binary| !is_nextest_binary_id(binary)) {
            return None;
        }
        Some(Self { path, binary, name })
    }

    fn short_name(&self) -> &str {
        self.name.rsplit("::").next().unwrap_or(self.name)
    }

    fn compatible_with(&self, other: &Self) -> bool {
        !matches!((self.path, other.path), (Some(a), Some(b)) if a != b)
            && !matches!((self.binary, other.binary), (Some(a), Some(b)) if a != b)
            && (self.name == other.name
                || ((!self.name.contains("::") || !other.name.contains("::"))
                    && self.short_name() == other.short_name()))
    }
}

fn is_nextest_binary_id(binary: &str) -> bool {
    // Nextest identifies libraries as `package`, integration tests as
    // `package::target`, and other targets as `package::kind/target`.
    // Validate those separators separately from Rust module qualification and
    // retain the whole binary ID, including the kind and target, for matching.
    let valid_component = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c == '_' || c == '-' || c.is_alphanumeric())
    };
    let Some((package, target)) = binary.split_once("::") else {
        return valid_component(binary);
    };
    valid_component(package)
        && match target.split_once('/') {
            Some((kind, target)) => valid_component(kind) && valid_component(target),
            None => valid_component(target),
        }
}

fn parse_test_reference(reference: &str) -> std::result::Result<TestReference, String> {
    let reference = reference.trim().trim_matches('`').trim();
    if reference.is_empty() {
        return Err("reference is empty".to_string());
    }
    if let Some(command) = parse_python_test_command(reference)? {
        return Ok(command);
    }
    let (path, selector) = if let Some(prefix) = reference.strip_suffix(')') {
        if let Some((path, selector)) = prefix.rsplit_once(" (") {
            (path.trim(), Some(selector.trim().to_string()))
        } else {
            (reference, None)
        }
    } else if let Some((path, selector)) = reference.split_once("::") {
        (path.trim(), Some(selector.trim().to_string()))
    } else {
        (reference, None)
    };
    // Planning documents commonly format repository paths as inline Markdown.
    // The backticks are presentation, not part of the committed path that Git
    // should validate.
    let path = path.trim_matches('`').trim();
    validate_test_path(path)?;
    if selector
        .as_ref()
        .is_some_and(|selector| selector.is_empty())
    {
        return Err("test identifier is empty".to_string());
    }
    Ok(TestReference {
        path: path.to_string(),
        selector,
        command: None,
    })
}

/// Resolve one exact command-shaped Test reference that a rejected final
/// candidate gate could not find in canonical Tester evidence. Selector-only
/// diagnostics remain code/test-contract corrections rather than host runs.
pub(crate) fn requested_host_evidence_command(evidence: &CandidateGateEvidence) -> Option<String> {
    if evidence.schema_version == 0
        || evidence.schema_version > CANDIDATE_GATE_SCHEMA_VERSION
        || evidence.phase != CandidateGatePhase::FinalTester
        || evidence.disposition != CandidateGateDisposition::Rejected
    {
        return None;
    }
    let mut blocking = evidence.checks.iter().filter(|check| {
        matches!(
            check.status,
            CandidateCheckStatus::Failed | CandidateCheckStatus::Error
        )
    });
    let tester = blocking.next()?;
    if tester.name != "tester-reference-evidence"
        || tester.status != CandidateCheckStatus::Failed
        || blocking.next().is_some()
    {
        return None;
    }
    let mut commands = tester
        .diagnostics
        .iter()
        .filter_map(|diagnostic| {
            diagnostic
                .strip_prefix("no passing Tester result matches ")
                .and_then(|reference| parse_test_reference(reference).ok())
                .and_then(|reference| reference.command)
        })
        .collect::<Vec<_>>();
    commands.sort();
    commands.dedup();
    let [command] = commands.as_slice() else {
        return None;
    };
    Some(command.clone())
}

fn parse_python_test_command(
    reference: &str,
) -> std::result::Result<Option<TestReference>, String> {
    let fields = reference.split_whitespace().collect::<Vec<_>>();
    let mut executable = 0;
    while fields
        .get(executable)
        .is_some_and(|field| is_environment_assignment(field))
    {
        executable += 1;
    }
    let Some(launcher) = fields.get(executable) else {
        return Ok(None);
    };
    if !is_python_launcher(launcher) {
        return Ok(None);
    }
    if fields
        .iter()
        .any(|field| field.contains(['\'', '"', '\\', ';', '|', '&', '<', '>']))
    {
        return Err("Python command must not contain quoting or shell operators".to_string());
    }
    let mut operand = executable + 1;
    while let Some(field) = fields.get(operand) {
        if *field == "--" {
            operand += 1;
            break;
        }
        if matches!(
            *field,
            "-B" | "-b"
                | "-bb"
                | "-d"
                | "-E"
                | "-I"
                | "-O"
                | "-OO"
                | "-P"
                | "-q"
                | "-s"
                | "-S"
                | "-u"
                | "-v"
                | "-V"
                | "-x"
        ) {
            operand += 1;
            continue;
        }
        if field.starts_with('-') {
            return Err(format!(
                "unsupported Python option {field:?}; Test: must name a project script directly"
            ));
        }
        break;
    }
    let path = fields
        .get(operand)
        .ok_or_else(|| "Python command has no project script operand".to_string())?;
    validate_test_path(path)?;
    Ok(Some(TestReference {
        path: (*path).to_string(),
        selector: None,
        command: Some(reference.to_string()),
    }))
}

fn is_environment_assignment(field: &str) -> bool {
    let Some((name, _)) = field.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn is_python_launcher(field: &str) -> bool {
    if field == "python" {
        return true;
    }
    field.strip_prefix("python").is_some_and(|version| {
        version.chars().any(|character| character.is_ascii_digit())
            && version
                .chars()
                .all(|character| character.is_ascii_digit() || character == '.')
    })
}

fn validate_test_path(path: &str) -> std::result::Result<(), String> {
    let parsed = Path::new(path);
    if parsed.is_absolute()
        || path.is_empty()
        || parsed
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("path must be a normalized project-relative path".to_string());
    }
    Ok(())
}

fn check_tester_evidence(
    request: &CandidateGateRequest<'_>,
    references: &[TestReference],
) -> Result<CandidateCheckEvidence> {
    let Some(path) = request.tester_results_path else {
        return Ok(check(
            "tester-reference-evidence",
            CandidateCheckStatus::Skipped,
            vec!["Final Tester evidence is not available at this frontier".to_string()],
        ));
    };
    let source = match fs::read(path) {
        Ok(source) => source,
        Err(error) => {
            return Ok(check(
                "tester-reference-evidence",
                CandidateCheckStatus::Error,
                vec![format!("cannot read {}: {error}", path.display())],
            ));
        }
    };
    let results: crate::tester::TesterResults = match serde_json::from_slice(&source) {
        Ok(results) => results,
        Err(error) => {
            return Ok(check(
                "tester-reference-evidence",
                CandidateCheckStatus::Error,
                vec![format!("cannot parse {}: {error}", path.display())],
            ));
        }
    };
    let Some(evidence_commit) = results.candidate_commit.as_deref() else {
        return Ok(check(
            "tester-reference-evidence",
            CandidateCheckStatus::Error,
            vec!["Tester evidence is not bound to a candidate commit".to_string()],
        ));
    };
    if evidence_commit != request.candidate_commit {
        let acceptable_learner_delta = request.phase == CandidateGatePhase::Learner
            && learner_only_delta(request.workspace, evidence_commit, request.candidate_commit)?;
        if !acceptable_learner_delta {
            return Ok(check(
                "tester-reference-evidence",
                CandidateCheckStatus::Error,
                vec![format!(
                    "Tester evidence names {evidence_commit}, not candidate {}",
                    request.candidate_commit
                )],
            ));
        }
    }
    // Non-passing identities participate in resolution too. Filtering them
    // out first would let whichever same-named test passed define the match.
    let rust_tests = results
        .tests
        .iter()
        .filter_map(|test| RustTestIdentity::parse(&test.id).map(|identity| (identity, test)))
        .collect::<Vec<_>>();
    let python_tests = results
        .tests
        .iter()
        .filter_map(|test| PythonTestIdentity::parse(&test.id).map(|identity| (identity, test)))
        .collect::<Vec<_>>();
    let mut missing_references = Vec::new();
    for reference in references {
        let matched = if let Some(expected_command) = reference.command.as_deref() {
            request
                .accepted_tester_commands
                .iter()
                .any(|command| command.trim() == expected_command)
                || results.commands.iter().any(|command| {
                    command.exit_code == 0 && command.command.trim() == expected_command
                })
        } else if reference.path.ends_with(".rs") && reference.selector.is_some() {
            reference
                .rust_identity()
                .is_some_and(|expected| rust_evidence_matches_reference(&expected, &rust_tests))
        } else if reference.path.ends_with(".py") && reference.selector.is_some() {
            reference
                .python_identity()
                .is_some_and(|expected| python_evidence_matches_reference(&expected, &python_tests))
        } else {
            let mut matching = results
                .tests
                .iter()
                .filter(|test| test_result_matches_reference(test, reference))
                .peekable();
            matching.peek().is_some() && matching.all(|test| test.status == "pass")
        };
        if !matched {
            let label = reference.command.clone().unwrap_or_else(|| {
                reference.selector.as_deref().map_or_else(
                    || reference.path.clone(),
                    |selector| format!("{} ({selector})", reference.path),
                )
            });
            missing_references.push(format!("no passing Tester result matches {label}"));
            if reference.command.is_none() {
                let expected = reference.rust_identity();
                let relevant = results
                    .tests
                    .iter()
                    .filter(|test| {
                        // This selects diagnostic context only; matching above
                        // always uses parsed identities or an exact script path.
                        expected
                            .as_ref()
                            .is_some_and(|identity| test.id.contains(identity.short_name()))
                            || test.id == reference.path
                    })
                    .collect::<Vec<_>>();
                let observed = if relevant.is_empty() {
                    results.tests.iter().collect::<Vec<_>>()
                } else {
                    relevant
                };
                let identities = observed
                    .iter()
                    .take(20)
                    .map(|test| format!("{:?} ({})", test.id, test.status))
                    .collect::<Vec<_>>()
                    .join(", ");
                missing_references.push(format!(
                    "observed Tester identities for {label} (showing {} of {}): {identities}",
                    observed.len().min(20),
                    observed.len()
                ));
            }
        }
    }
    let mut diagnostics = Vec::new();
    if !missing_references.is_empty() {
        let artifact_dir = path.parent().unwrap_or(path);
        for (index, command) in results.commands.iter().enumerate().filter(|(_, command)| {
            command.exit_code != 0
                && !request
                    .accepted_tester_commands
                    .iter()
                    .any(|accepted| accepted.trim() == command.command.trim())
        }) {
            diagnostics.push(format!(
                "Tester command {index} exited {}: {}",
                command.exit_code, command.command
            ));
            diagnostics.push(format!(
                "Tester command {index} stdout log: {}",
                artifact_dir.join(&command.stdout_log).display()
            ));
            diagnostics.push(format!(
                "Tester command {index} stderr log: {}",
                artifact_dir.join(&command.stderr_log).display()
            ));
        }
        diagnostics.extend(missing_references);
    }
    let status = if diagnostics.is_empty() {
        CandidateCheckStatus::Passed
    } else {
        CandidateCheckStatus::Failed
    };
    Ok(check("tester-reference-evidence", status, diagnostics))
}

fn rust_evidence_matches_reference(
    expected: &RustTestIdentity<'_>,
    tests: &[(RustTestIdentity<'_>, &crate::tester::TestResult)],
) -> bool {
    let matching = tests
        .iter()
        .filter(|(identity, _)| expected.compatible_with(identity))
        .collect::<Vec<_>>();
    !matching.is_empty()
        && matching.iter().all(|(identity, test)| {
            test.status == "pass"
                && matching.iter().all(|(other, _)| identity.compatible_with(other))
                // A result that omitted qualifiers cannot stand in for this
                // reference if it could instead name a conflicting identity.
                && tests.iter().all(|(other, _)| {
                    !identity.compatible_with(other) || expected.compatible_with(other)
                })
        })
}

fn python_evidence_matches_reference(
    expected: &PythonTestIdentity<'_>,
    tests: &[(PythonTestIdentity<'_>, &crate::tester::TestResult)],
) -> bool {
    let matching = tests
        .iter()
        .filter(|(identity, _)| expected.compatible_with(identity))
        .collect::<Vec<_>>();
    !matching.is_empty()
        && matching.iter().all(|(identity, test)| {
            test.status == "pass"
                && matching
                    .iter()
                    .all(|(other, _)| identity.compatible_with(other))
                && tests.iter().all(|(other, _)| {
                    !identity.compatible_with(other) || expected.compatible_with(other)
                })
        })
}

fn test_result_matches_reference(
    test: &crate::tester::TestResult,
    reference: &TestReference,
) -> bool {
    if reference.path.ends_with(".sh") {
        // The structured result's harness label is project-defined. Only the
        // exact script path identifies this test, never its coverage prose.
        return test.id == reference.path;
    }
    if test.test_harness == "shell-harness" && test.id == reference.path {
        return true;
    }
    match reference.selector.as_deref() {
        Some(selector) => {
            // Cargo/nextest extractors in downstream projects commonly retain
            // the source path in a human-readable identity such as
            // `src/lib.rs (drain_is_idempotent)`. Treat that as the same
            // selector represented by the documented `path (selector)`
            // reference, while preserving the existing bare, `::`, and `$`
            // forms.
            test.id == selector
                || test.id == format!("{} ({selector})", reference.path)
                || test.id.ends_with(&format!("::{selector}"))
                || test.id.ends_with(&format!("${selector}"))
                || test
                    .id
                    .split_whitespace()
                    .next_back()
                    .is_some_and(|identity| identity == selector)
        }
        None => test.id == reference.path,
    }
}

fn learner_only_delta(workspace: &Path, from: &str, to: &str) -> Result<bool> {
    let output = crate::git::run_raw(workspace, &["diff", "--name-only", from, to])?;
    if !output.status.success() {
        return Ok(false);
    }
    let paths = String::from_utf8_lossy(&output.stdout);
    Ok(!paths.trim().is_empty()
        && paths
            .lines()
            .all(|path| path.starts_with(".fluent/expertise/")))
}

fn run_project_check(
    request: &CandidateGateRequest<'_>,
    base_commit: &str,
) -> Result<CandidateCheckEvidence> {
    if crate::hooks::find_hook(request.project_root, "check-pre-merge").is_none() {
        return Ok(check(
            "project-check",
            CandidateCheckStatus::Skipped,
            vec!["Project has no executable check-pre-merge hook".to_string()],
        ));
    }
    let before_head = crate::git::run_stdout(
        request.workspace,
        &["rev-parse", "HEAD"],
        "capture project-check HEAD",
    )?;
    let before_status = crate::git::run_stdout(
        request.workspace,
        &["status", "--porcelain", "--untracked-files=all"],
        "capture project-check status",
    )?;
    let log_dir = request
        .artifact_path
        .parent()
        .context("candidate gate artifact path has no parent")?
        .join("hooks");
    let context = crate::hooks::HookContext {
        work_item_id: Some(request.work_item_id.to_string()),
        attempt_id: Some(request.attempt_id.to_string()),
        task_id: request.task_id.map(str::to_string),
        candidate_commit: Some(request.candidate_commit.to_string()),
        base_commit: Some(base_commit.to_string()),
        artifact_dir: request.artifact_path.parent().map(Path::to_path_buf),
        log_dir,
        ..Default::default()
    };
    let outcome = match crate::hooks::run_hook(
        request.project_root,
        "check-pre-merge",
        request.workspace,
        &context,
    ) {
        Ok(Some(outcome)) => outcome,
        Ok(None) => {
            return Ok(check(
                "project-check",
                CandidateCheckStatus::Error,
                vec!["check-pre-merge disappeared before launch".to_string()],
            ));
        }
        Err(error) => {
            return Ok(check(
                "project-check",
                CandidateCheckStatus::Error,
                vec![format!("check-pre-merge could not run: {error:#}")],
            ));
        }
    };
    let after_head = crate::git::run_stdout(
        request.workspace,
        &["rev-parse", "HEAD"],
        "verify project-check HEAD",
    )?;
    let after_status = crate::git::run_stdout(
        request.workspace,
        &["status", "--porcelain", "--untracked-files=all"],
        "verify project-check status",
    )?;
    let log_path = Some(evidence_path(request.project_root, &outcome.log_path));
    if before_head != after_head || before_status != after_status {
        return Ok(CandidateCheckEvidence {
            name: "project-check".to_string(),
            status: CandidateCheckStatus::Error,
            diagnostics: vec![
                "check-pre-merge changed candidate HEAD or worktree state".to_string(),
            ],
            log_path,
        });
    }
    Ok(CandidateCheckEvidence {
        name: "project-check".to_string(),
        status: if outcome.passed {
            CandidateCheckStatus::Passed
        } else {
            CandidateCheckStatus::Failed
        },
        diagnostics: (!outcome.passed)
            .then(|| format!("check-pre-merge exited {}", outcome.exit_code))
            .into_iter()
            .collect(),
        log_path,
    })
}

fn check(
    name: &str,
    status: CandidateCheckStatus,
    diagnostics: Vec<String>,
) -> CandidateCheckEvidence {
    CandidateCheckEvidence {
        name: name.to_string(),
        status,
        diagnostics,
        log_path: None,
    }
}

fn output_diagnostics(output: &std::process::Output) -> Vec<String> {
    let mut diagnostics = Vec::new();
    for source in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(source);
        diagnostics.extend(
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_string),
        );
    }
    if diagnostics.is_empty() {
        diagnostics.push(format!(
            "command exited {}",
            output
                .status
                .code()
                .map_or_else(|| "after a signal".to_string(), |code| code.to_string())
        ));
    }
    diagnostics
}

fn evidence_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_model::{
        WRITER_COMPLETION_CONTRACT_VERSION, WriterCompletionRequirement,
        WriterCompletionRequirementKind,
    };
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        project: tempfile::TempDir,
        root: tempfile::TempDir,
        artifacts: tempfile::TempDir,
        base: String,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::TempDir::new().unwrap();
            crate::git::run(root.path(), &["init", "-q"], "initialize gate fixture").unwrap();
            crate::git::run(
                root.path(),
                &["config", "user.name", "Test Author"],
                "configure fixture author",
            )
            .unwrap();
            crate::git::run(
                root.path(),
                &["config", "user.email", "test@example.com"],
                "configure fixture email",
            )
            .unwrap();
            fs::write(root.path().join("README.md"), "fixture\n").unwrap();
            crate::git::run(root.path(), &["add", "README.md"], "stage fixture").unwrap();
            crate::git::run(
                root.path(),
                &["commit", "-q", "-m", "Create fixture"],
                "commit fixture",
            )
            .unwrap();
            let base =
                crate::git::run_stdout(root.path(), &["rev-parse", "HEAD"], "read base").unwrap();
            Self {
                project: tempfile::TempDir::new().unwrap(),
                root,
                artifacts: tempfile::TempDir::new().unwrap(),
                base,
            }
        }

        fn commit(&self, path: &str, contents: &str, message: &str) -> String {
            let path = self.root.path().join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, contents).unwrap();
            crate::git::run(self.root.path(), &["add", "-A"], "stage candidate").unwrap();
            crate::git::run(
                self.root.path(),
                &["commit", "-q", "-m", message],
                "commit candidate",
            )
            .unwrap();
            crate::git::run_stdout(self.root.path(), &["rev-parse", "HEAD"], "read candidate")
                .unwrap()
        }

        fn legacy_literal_newline_gate(
            &self,
            candidate: &str,
            artifact: &Path,
        ) -> CandidateGateEvidence {
            let mut evidence =
                run_candidate_gate(self.request(candidate, None, None, artifact)).unwrap();
            // Older Fluent versions rejected this valid Git message content.
            let metadata = evidence
                .checks
                .iter_mut()
                .find(|check| check.name == "commit-metadata")
                .unwrap();
            metadata.status = CandidateCheckStatus::Failed;
            metadata.diagnostics = vec![format!(
                "commit {candidate} contains literal \\n text instead of line breaks"
            )];
            evidence.disposition = CandidateGateDisposition::Rejected;
            fs::write(artifact, serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
            evidence
        }

        fn request<'a>(
            &'a self,
            candidate: &'a str,
            contract: Option<&'a WriterCompletionContract>,
            tester_results_path: Option<&'a Path>,
            artifact_path: &'a Path,
        ) -> CandidateGateRequest<'a> {
            CandidateGateRequest {
                project_root: self.project.path(),
                workspace: self.root.path(),
                work_item_id: "work-1",
                attempt_id: "attempt-1",
                task_id: Some("attempt-1-write-1"),
                phase: CandidateGatePhase::Writer,
                base_commit: &self.base,
                candidate_commit: candidate,
                completion_contract: contract,
                allow_tree_identical_metadata_rewrite: false,
                metadata_rewrite_authorization: None,
                tester_results_path,
                accepted_tester_commands: &[],
                artifact_path,
            }
        }
    }

    fn behavior_contract(reference: &str) -> WriterCompletionContract {
        WriterCompletionContract {
            version: WRITER_COMPLETION_CONTRACT_VERSION,
            requirements: vec![WriterCompletionRequirement {
                responsibility: None,
                id: "behavior:area:b1".to_string(),
                kind: WriterCompletionRequirementKind::Behavior,
                source: "behaviors.md#area:b1".to_string(),
                requirement: "THE SYSTEM SHALL work.".to_string(),
                verification_refs: vec![reference.to_string()],
                command: None,
            }],
        }
    }

    fn check<'a>(evidence: &'a CandidateGateEvidence, name: &str) -> &'a CandidateCheckEvidence {
        evidence
            .checks
            .iter()
            .find(|check| check.name == name)
            .unwrap()
    }

    #[test]
    fn parses_supported_test_reference_forms() {
        let double_colon = parse_test_reference("tests/example.rs::module::works").unwrap();
        assert_eq!(double_colon.path, "tests/example.rs");
        assert_eq!(double_colon.selector.as_deref(), Some("module::works"));
        assert!(double_colon.command.is_none());

        let parenthesized = parse_test_reference("tests/example.rs (module::works)").unwrap();
        assert_eq!(parenthesized.path, "tests/example.rs");
        assert_eq!(parenthesized.selector.as_deref(), Some("module::works"));
        assert!(parenthesized.command.is_none());

        let path = parse_test_reference("tests/behaviors/example.sh").unwrap();
        assert_eq!(path.path, "tests/behaviors/example.sh");
        assert!(path.selector.is_none());
        assert!(path.command.is_none());
        assert!(parse_test_reference("tests/example.rs::").is_err());
    }

    #[test]
    fn derived_hook_classifier_normalizes_project_body_line_wording() {
        assert_eq!(
            normalize_metadata_reason("body line 2 exceeds 72 characters", true),
            Some("literal-line-breaks".to_string())
        );
        assert_eq!(
            normalize_metadata_reason("body line 0 exceeds 72 characters", true),
            None
        );
        assert_eq!(
            normalize_metadata_reason("body line 2 exceeds 73 characters", true),
            None
        );
        assert_eq!(
            normalize_metadata_reason(
                "has no body separated from its subject by a blank line",
                true,
            ),
            None
        );
    }

    #[test]
    fn parses_markdown_wrapped_source_path_test_reference() {
        let reference = parse_test_reference(
            "`src/candidate_metadata_recovery.rs` (metadata_recovery_accepts_derived_project_check)",
        )
        .unwrap();

        assert_eq!(reference.path, "src/candidate_metadata_recovery.rs");
        assert_eq!(
            reference.selector.as_deref(),
            Some("metadata_recovery_accepts_derived_project_check")
        );
        assert!(reference.command.is_none());
    }

    #[test]
    fn validates_markdown_wrapped_source_path_test_reference() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "src/candidate_metadata_recovery.rs",
            "#[test]\nfn metadata_recovery_accepts_derived_project_check() {}\n",
            "Add recovery test\n\n- Cover derived metadata checks",
        );
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract(
            "`src/candidate_metadata_recovery.rs` (metadata_recovery_accepts_derived_project_check)",
        );

        let evidence =
            run_candidate_gate(fixture.request(&candidate, Some(&contract), None, &artifact))
                .unwrap();

        assert_eq!(
            check(&evidence, "behavior-references").status,
            CandidateCheckStatus::Passed,
            "{:?}",
            check(&evidence, "behavior-references").diagnostics
        );
    }

    #[test]
    fn parses_python_command_reference_as_committed_script() {
        let reference = parse_test_reference(
            "PYTHONDONTWRITEBYTECODE=1 python3 -I tests/fluent_merge_readiness.py",
        )
        .unwrap();

        assert_eq!(reference.path, "tests/fluent_merge_readiness.py");
        assert!(reference.selector.is_none());
        assert_eq!(
            reference.command.as_deref(),
            Some("PYTHONDONTWRITEBYTECODE=1 python3 -I tests/fluent_merge_readiness.py")
        );
        assert!(parse_test_reference("python3 -m pytest tests/readiness.py").is_err());
        assert!(parse_test_reference("python3 tests/readiness.py | tee output").is_err());
    }

    #[test]
    fn python_command_reference_validates_committed_script_operand() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/fluent_merge_readiness.py",
            "#!/usr/bin/env python3\n",
            "Add readiness journey\n\n- Cover the public workflow",
        );
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract("python3 tests/fluent_merge_readiness.py");

        let evidence =
            run_candidate_gate(fixture.request(&candidate, Some(&contract), None, &artifact))
                .unwrap();

        assert_eq!(
            check(&evidence, "behavior-references").status,
            CandidateCheckStatus::Passed
        );
    }

    #[test]
    fn markdown_wrapped_test_reference_validates_committed_path_and_selector() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "src/review.rs",
            "fn evidence_needed_requires_actionable_host_reason() {}\n",
            "Add review regression\n\n- Cover actionable host evidence",
        );
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract =
            behavior_contract("`src/review.rs` (evidence_needed_requires_actionable_host_reason)");

        let evidence =
            run_candidate_gate(fixture.request(&candidate, Some(&contract), None, &artifact))
                .unwrap();

        assert_eq!(
            check(&evidence, "behavior-references").status,
            CandidateCheckStatus::Passed
        );
    }

    #[test]
    fn python_command_reference_requires_committed_script_operand() {
        let fixture = Fixture::new();
        let candidate = fixture.commit("README.md", "fixture changed\n", "Update fixture");
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract("python3 tests/missing.py");

        let evidence =
            run_candidate_gate(fixture.request(&candidate, Some(&contract), None, &artifact))
                .unwrap();

        assert!(
            check(&evidence, "behavior-references")
                .diagnostics
                .iter()
                .any(|line| line.contains("missing committed test path tests/missing.py"))
        );
    }

    #[test]
    fn accepts_literal_backslash_n_as_project_policy() {
        for message in [
            "Add value\\n\\nExplain value",
            "Add value\n\n- Explain literal \\n text",
        ] {
            let fixture = Fixture::new();
            let candidate = fixture.commit("src/lib.rs", "pub fn value() {}\n", message);
            let artifact = fixture.artifacts.path().join("candidate-gate.json");

            let evidence =
                run_candidate_gate(fixture.request(&candidate, None, None, &artifact)).unwrap();

            assert_eq!(evidence.disposition, CandidateGateDisposition::Passed);
            assert_eq!(
                check(&evidence, "commit-metadata").status,
                CandidateCheckStatus::Passed
            );
            assert!(check(&evidence, "commit-metadata").diagnostics.is_empty());
            assert!(evidence.metadata_rewrite.is_none());
        }
    }

    #[test]
    fn literal_backslash_n_does_not_mask_blocking_metadata() {
        for defect in ["identity", "empty subject"] {
            let fixture = Fixture::new();
            if defect == "identity" {
                crate::git::run(
                    fixture.root.path(),
                    &["config", "user.email", "writer@example.invalid"],
                    "set invalid identity",
                )
                .unwrap();
            }
            let mut candidate = fixture.commit(
                "src/lib.rs",
                "pub fn value() {}\n",
                "Add value\\n\\nExplain value",
            );
            if defect == "empty subject" {
                crate::git::run(
                    fixture.root.path(),
                    &[
                        "commit",
                        "--amend",
                        "-q",
                        "--cleanup=verbatim",
                        "--allow-empty-message",
                        "-m",
                        "\n\nExplain literal \\n text",
                    ],
                    "create empty subject",
                )
                .unwrap();
                candidate = crate::git::run_stdout(
                    fixture.root.path(),
                    &["rev-parse", "HEAD"],
                    "read candidate",
                )
                .unwrap();
            }
            let artifact = fixture.artifacts.path().join("candidate-gate.json");

            let evidence =
                run_candidate_gate(fixture.request(&candidate, None, None, &artifact)).unwrap();

            assert_eq!(
                evidence.disposition,
                CandidateGateDisposition::Rejected,
                "{defect}"
            );
            let metadata = check(&evidence, "commit-metadata");
            assert_eq!(metadata.status, CandidateCheckStatus::Failed);
            let expected = if defect == "identity" {
                vec!["invalid author email", "invalid committer email"]
            } else {
                vec!["has an empty subject"]
            };
            for reason in expected {
                assert!(
                    metadata
                        .diagnostics
                        .iter()
                        .any(|line| line.contains(reason)),
                    "{metadata:?}"
                );
            }
            assert!(
                metadata
                    .diagnostics
                    .iter()
                    .all(|line| !line.contains("literal"))
            );
        }
    }

    #[test]
    fn literal_backslash_n_does_not_mask_whitespace_or_project_failure() {
        for failed_check in ["whitespace", "project-check"] {
            let fixture = Fixture::new();
            let contents = if failed_check == "whitespace" {
                "pub fn value() {    \n}\n"
            } else {
                "pub fn value() {}\n"
            };
            let candidate = fixture.commit("src/lib.rs", contents, "Add value\\n\\nExplain value");
            if failed_check == "project-check" {
                let hooks = fixture.project.path().join(".fluent/hooks");
                fs::create_dir_all(&hooks).unwrap();
                let hook = hooks.join("check-pre-merge");
                fs::write(
                    &hook,
                    "#!/bin/sh\nprintf 'dependency cycle: app -> render -> app\\n'\nexit 9\n",
                )
                .unwrap();
                fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
            }
            let artifact = fixture.artifacts.path().join("candidate-gate.json");

            let evidence =
                run_candidate_gate(fixture.request(&candidate, None, None, &artifact)).unwrap();

            assert_eq!(evidence.disposition, CandidateGateDisposition::Rejected);
            assert_eq!(
                check(&evidence, "commit-metadata").status,
                CandidateCheckStatus::Passed
            );
            assert_eq!(
                check(&evidence, failed_check).status,
                CandidateCheckStatus::Failed
            );
            if failed_check == "project-check" {
                assert!(
                    fs::read_to_string(check(&evidence, failed_check).log_path.as_ref().unwrap())
                        .unwrap()
                        .contains("dependency cycle")
                );
            }
            let parsed: CandidateGateEvidence =
                serde_json::from_slice(&fs::read(artifact).unwrap()).unwrap();
            assert_eq!(parsed, evidence);
        }
    }

    #[test]
    fn rejects_merge_commits() {
        let merge_fixture = Fixture::new();
        let branch = crate::git::run_stdout(
            merge_fixture.root.path(),
            &["branch", "--show-current"],
            "read fixture branch",
        )
        .unwrap();
        merge_fixture.commit("src/main.rs", "pub fn main_value() {}\n", "Add main value");
        crate::git::run(
            merge_fixture.root.path(),
            &["checkout", "-q", "-b", "side", &merge_fixture.base],
            "create side branch",
        )
        .unwrap();
        merge_fixture.commit("src/side.rs", "pub fn side_value() {}\n", "Add side value");
        crate::git::run(
            merge_fixture.root.path(),
            &["checkout", "-q", &branch],
            "restore fixture branch",
        )
        .unwrap();
        crate::git::run(
            merge_fixture.root.path(),
            &[
                "merge",
                "--no-ff",
                "-q",
                "-m",
                "Merge side branch\\nExplain merge",
                "side",
            ],
            "create merge candidate",
        )
        .unwrap();
        let merge_candidate = crate::git::run_stdout(
            merge_fixture.root.path(),
            &["rev-parse", "HEAD"],
            "read merge candidate",
        )
        .unwrap();
        let merge_artifact = merge_fixture.artifacts.path().join("merge.json");
        let merge_evidence = run_candidate_gate(merge_fixture.request(
            &merge_candidate,
            None,
            None,
            &merge_artifact,
        ))
        .unwrap();
        assert!(
            check(&merge_evidence, "commit-metadata")
                .diagnostics
                .iter()
                .any(|line| line.contains("is a merge commit"))
        );
    }

    #[test]
    fn rejects_invalid_identities() {
        let identity_fixture = Fixture::new();
        crate::git::run(
            identity_fixture.root.path(),
            &["config", "user.email", "writer@example.invalid"],
            "configure invalid fixture identity",
        )
        .unwrap();
        let identity_candidate =
            identity_fixture.commit("src/lib.rs", "pub fn value() {}\n", "Add value");
        let identity_artifact = identity_fixture.artifacts.path().join("identity.json");
        let identity_evidence = run_candidate_gate(identity_fixture.request(
            &identity_candidate,
            None,
            None,
            &identity_artifact,
        ))
        .unwrap();
        assert!(
            check(&identity_evidence, "commit-metadata")
                .diagnostics
                .iter()
                .any(|line| line.contains("invalid author email"))
        );
    }

    #[test]
    fn rejects_an_invalid_append_only_ancestor() {
        let fixture = Fixture::new();
        crate::git::run(
            fixture.root.path(),
            &["config", "user.email", "writer@example.invalid"],
            "set invalid ancestor identity",
        )
        .unwrap();
        fixture.commit(
            "src/lib.rs",
            "pub fn value() {}\n",
            "Add value\\n\\nExplain value",
        );
        crate::git::run(
            fixture.root.path(),
            &["config", "user.email", "test@example.com"],
            "restore valid identity",
        )
        .unwrap();
        let corrected = fixture.commit(
            "src/lib.rs",
            "pub fn value() { println!(\"fixed\"); }\n",
            "Correct candidate metadata gate\n\n- Preserve the rejected commit as append-only history",
        );
        let artifact = fixture.artifacts.path().join("candidate-gate.json");

        let evidence =
            run_candidate_gate(fixture.request(&corrected, None, None, &artifact)).unwrap();

        assert_eq!(
            check(&evidence, "commit-metadata").status,
            CandidateCheckStatus::Failed
        );
    }

    #[test]
    fn accepts_tree_identical_metadata_rewrite_from_legacy_gate() {
        let fixture = Fixture::new();
        let rejected = fixture.commit(
            "src/lib.rs",
            "pub fn value() {}\n",
            "Add value\\n\\nExplain value",
        );
        let prior_artifact = fixture.artifacts.path().join("prior-gate.json");
        let prior = fixture.legacy_literal_newline_gate(&rejected, &prior_artifact);
        assert_eq!(prior.disposition, CandidateGateDisposition::Rejected);
        assert_eq!(
            prior
                .checks
                .iter()
                .filter(|check| check.status == CandidateCheckStatus::Failed)
                .map(|check| check.name.as_str())
                .collect::<Vec<_>>(),
            vec!["commit-metadata"]
        );
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&fs::read(&prior_artifact).unwrap()).unwrap();
        legacy["schema_version"] = serde_json::json!(1);
        legacy.as_object_mut().unwrap().remove("metadata_rewrite");
        fs::write(&prior_artifact, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        crate::git::run(
            fixture.root.path(),
            &[
                "commit",
                "--amend",
                "-q",
                "-m",
                "Add value\n\n- Explain value",
            ],
            "rewrite candidate metadata",
        )
        .unwrap();
        let replacement = crate::git::run_stdout(
            fixture.root.path(),
            &["rev-parse", "HEAD"],
            "read replacement",
        )
        .unwrap();
        let artifact = fixture.artifacts.path().join("replacement-gate.json");
        let mut request = fixture.request(&replacement, None, None, &artifact);
        request.base_commit = &rejected;

        let evidence = run_candidate_gate_with_prior(request, Some(&prior_artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Passed);
        assert_eq!(evidence.base_commit, fixture.base);
        assert_eq!(
            check(&evidence, "commit-metadata").status,
            CandidateCheckStatus::Passed
        );
        let persisted: CandidateGateEvidence =
            serde_json::from_slice(&fs::read(&artifact).unwrap()).unwrap();
        assert_eq!(persisted, evidence);
        let rewrite = persisted.metadata_rewrite.unwrap();
        assert_eq!(rewrite.original_base_commit, fixture.base);
        assert_eq!(rewrite.rejected_candidate_commit, rejected);
        assert_eq!(rewrite.replacement_candidate_commit, replacement);
        assert_eq!(
            rewrite.prior_gate_path.as_deref(),
            Some(prior_artifact.to_string_lossy().as_ref())
        );
        assert_eq!(
            rewrite.authorization,
            CandidateMetadataRewriteAuthorization::RejectedMetadataGate
        );
    }

    #[test]
    fn blocks_metadata_rewrite_when_the_candidate_tree_changes() {
        let fixture = Fixture::new();
        let rejected = fixture.commit(
            "src/lib.rs",
            "pub fn value() {}\n",
            "Add value\\n\\nExplain value",
        );
        let prior_artifact = fixture.artifacts.path().join("prior-gate.json");
        fixture.legacy_literal_newline_gate(&rejected, &prior_artifact);
        fs::write(
            fixture.root.path().join("src/lib.rs"),
            "pub fn value() { 1 }\n",
        )
        .unwrap();
        crate::git::run(fixture.root.path(), &["add", "src/lib.rs"], "stage rewrite").unwrap();
        crate::git::run(
            fixture.root.path(),
            &[
                "commit",
                "--amend",
                "-q",
                "-m",
                "Add value\n\n- Correct the candidate metadata",
            ],
            "rewrite candidate and tree",
        )
        .unwrap();
        let replacement = crate::git::run_stdout(
            fixture.root.path(),
            &["rev-parse", "HEAD"],
            "read replacement",
        )
        .unwrap();
        let artifact = fixture.artifacts.path().join("replacement-gate.json");
        let mut request = fixture.request(&replacement, None, None, &artifact);
        request.base_commit = &rejected;

        let evidence = run_candidate_gate_with_prior(request, Some(&prior_artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Blocked);
        assert!(evidence.metadata_rewrite.is_none());
        assert!(
            check(&evidence, "candidate-identity")
                .diagnostics
                .iter()
                .any(|line| line.contains("does not match rejected tree"))
        );
    }

    #[test]
    fn blocks_metadata_rewrite_when_commit_author_changes() {
        let fixture = Fixture::new();
        let rejected = fixture.commit(
            "src/lib.rs",
            "pub fn value() {}\n",
            "Add value\\n\\nExplain value",
        );
        let prior_artifact = fixture.artifacts.path().join("prior-gate.json");
        fixture.legacy_literal_newline_gate(&rejected, &prior_artifact);

        crate::git::run(
            fixture.root.path(),
            &[
                "-c",
                "user.name=Different Author",
                "-c",
                "user.email=different@example.com",
                "commit",
                "--amend",
                "--reset-author",
                "-q",
                "-m",
                "Add value\n\n- Explain value",
            ],
            "rewrite candidate with a different author",
        )
        .unwrap();
        let replacement = crate::git::run_stdout(
            fixture.root.path(),
            &["rev-parse", "HEAD"],
            "read replacement",
        )
        .unwrap();
        let artifact = fixture.artifacts.path().join("replacement-gate.json");
        let mut request = fixture.request(&replacement, None, None, &artifact);
        request.base_commit = &rejected;

        let evidence = run_candidate_gate_with_prior(request, Some(&prior_artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Blocked);
        assert!(
            check(&evidence, "candidate-identity")
                .diagnostics
                .iter()
                .any(|line| line.contains("tree or author identity"))
        );
    }

    #[test]
    fn blocks_metadata_rewrite_when_history_shape_changes() {
        let fixture = Fixture::new();
        let rejected = fixture.commit(
            "src/lib.rs",
            "pub fn value() {}\n",
            "Add value\\n\\nExplain value",
        );
        let prior_artifact = fixture.artifacts.path().join("prior-gate.json");
        fixture.legacy_literal_newline_gate(&rejected, &prior_artifact);

        crate::git::run(
            fixture.root.path(),
            &["reset", "--hard", &fixture.base],
            "rebuild metadata rewrite suffix",
        )
        .unwrap();
        fs::create_dir_all(fixture.root.path().join("src")).unwrap();
        fs::write(
            fixture.root.path().join("src/lib.rs"),
            "pub fn value() {}\n",
        )
        .unwrap();
        crate::git::run(
            fixture.root.path(),
            &["add", "src/lib.rs"],
            "stage replacement",
        )
        .unwrap();
        crate::git::run(
            fixture.root.path(),
            &["commit", "-q", "-m", "Add value\n\n- Explain value"],
            "create first replacement commit",
        )
        .unwrap();
        crate::git::run(
            fixture.root.path(),
            &[
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "Record metadata repair",
            ],
            "create extra replacement commit",
        )
        .unwrap();
        let replacement = crate::git::run_stdout(
            fixture.root.path(),
            &["rev-parse", "HEAD"],
            "read replacement",
        )
        .unwrap();
        let artifact = fixture.artifacts.path().join("replacement-gate.json");
        let mut request = fixture.request(&replacement, None, None, &artifact);
        request.base_commit = &rejected;

        let evidence = run_candidate_gate_with_prior(request, Some(&prior_artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Blocked);
        assert!(
            check(&evidence, "candidate-identity")
                .diagnostics
                .iter()
                .any(|line| line.contains("suffix length"))
        );
    }

    #[test]
    fn blocks_history_rewrite_after_mixed_gate_failures() {
        let fixture = Fixture::new();
        let rejected = fixture.commit(
            "src/lib.rs",
            "pub fn value() {    \n}\n",
            "Add value\\n\\nExplain value",
        );
        let prior_artifact = fixture.artifacts.path().join("prior-gate.json");
        fixture.legacy_literal_newline_gate(&rejected, &prior_artifact);
        crate::git::run(
            fixture.root.path(),
            &[
                "commit",
                "--amend",
                "-q",
                "-m",
                "Add value\n\n- Keep the candidate tree unchanged",
            ],
            "rewrite candidate metadata",
        )
        .unwrap();
        let replacement = crate::git::run_stdout(
            fixture.root.path(),
            &["rev-parse", "HEAD"],
            "read replacement",
        )
        .unwrap();
        let artifact = fixture.artifacts.path().join("replacement-gate.json");
        let mut request = fixture.request(&replacement, None, None, &artifact);
        request.base_commit = &rejected;

        let evidence = run_candidate_gate_with_prior(request, Some(&prior_artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Blocked);
        let diagnostics = check(&evidence, "candidate-identity")
            .diagnostics
            .join("\n");
        assert!(diagnostics.contains("candidate history changed"));
        assert!(diagnostics.contains("whitespace"));
        assert!(diagnostics.contains("commit-metadata"));
    }

    #[test]
    fn blocks_non_ancestor_candidate_without_prior_gate() {
        let fixture = Fixture::new();
        let rejected = fixture.commit("src/lib.rs", "pub fn value() {}\n", "Add bad value");
        crate::git::run(
            fixture.root.path(),
            &["commit", "--amend", "-q", "-m", "Add value"],
            "rewrite candidate",
        )
        .unwrap();
        let replacement = crate::git::run_stdout(
            fixture.root.path(),
            &["rev-parse", "HEAD"],
            "read replacement",
        )
        .unwrap();
        let artifact = fixture.artifacts.path().join("replacement-gate.json");
        let mut request = fixture.request(&replacement, None, None, &artifact);
        request.base_commit = &rejected;

        let evidence = run_candidate_gate(request).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Blocked);
    }

    #[test]
    fn recognizes_only_tree_identical_protected_base_ancestry_failures() {
        let protected = "a".repeat(40);
        let rejected = "b".repeat(40);
        let candidate = "c".repeat(40);
        let diagnostic =
            format!("original base commit {protected} is not an ancestor of candidate {candidate}");
        let mut evidence = CandidateGateEvidence {
            schema_version: CANDIDATE_GATE_SCHEMA_VERSION,
            phase: CandidateGatePhase::Writer,
            disposition: CandidateGateDisposition::Blocked,
            base_commit: protected.clone(),
            candidate_commit: candidate.clone(),
            tester_results_path: None,
            metadata_rewrite: Some(CandidateMetadataRewriteEvidence {
                prior_gate_path: Some(".fluent/work/prior/candidate-gate.json".to_string()),
                authorization: CandidateMetadataRewriteAuthorization::RejectedMetadataGate,
                original_base_commit: protected.clone(),
                rejected_candidate_commit: rejected.clone(),
                replacement_candidate_commit: candidate.clone(),
                candidate_tree: "tree".to_string(),
            }),
            checks: vec![
                CandidateCheckEvidence {
                    name: "candidate-identity".to_string(),
                    status: CandidateCheckStatus::Error,
                    diagnostics: vec![diagnostic],
                    log_path: None,
                },
                CandidateCheckEvidence {
                    name: "whitespace".to_string(),
                    status: CandidateCheckStatus::Passed,
                    diagnostics: Vec::new(),
                    log_path: None,
                },
                CandidateCheckEvidence {
                    name: "commit-metadata".to_string(),
                    status: CandidateCheckStatus::Passed,
                    diagnostics: Vec::new(),
                    log_path: None,
                },
                CandidateCheckEvidence {
                    name: "behavior-references".to_string(),
                    status: CandidateCheckStatus::Passed,
                    diagnostics: Vec::new(),
                    log_path: None,
                },
                CandidateCheckEvidence {
                    name: "tester-reference-evidence".to_string(),
                    status: CandidateCheckStatus::Skipped,
                    diagnostics: Vec::new(),
                    log_path: None,
                },
                CandidateCheckEvidence {
                    name: "project-check".to_string(),
                    status: CandidateCheckStatus::Skipped,
                    diagnostics: Vec::new(),
                    log_path: None,
                },
            ],
        };

        let authorization = candidate_ancestry_repair_authorization(&evidence).unwrap();
        assert_eq!(authorization.protected_base_commit, protected);
        assert_eq!(authorization.rejected_candidate_commit, rejected);
        assert_eq!(authorization.candidate_commit, candidate);

        evidence.checks[1].status = CandidateCheckStatus::Failed;
        assert!(candidate_ancestry_repair_authorization(&evidence).is_none());
    }

    #[test]
    fn accepts_tree_identical_rewrite_for_completion_repair() {
        let fixture = Fixture::new();
        let rejected = fixture.commit(
            "src/lib.rs",
            "pub fn value() {}\n",
            "Add value\n\n- Keep the candidate tree unchanged",
        );
        crate::git::run(
            fixture.root.path(),
            &[
                "commit",
                "--amend",
                "-q",
                "-m",
                "Add value\n\n- Correct the candidate metadata",
            ],
            "rewrite candidate metadata",
        )
        .unwrap();
        let replacement = crate::git::run_stdout(
            fixture.root.path(),
            &["rev-parse", "HEAD"],
            "read replacement",
        )
        .unwrap();
        let artifact = fixture.artifacts.path().join("replacement-gate.json");
        let mut request = fixture.request(&replacement, None, None, &artifact);
        request.base_commit = &rejected;
        request.allow_tree_identical_metadata_rewrite = true;

        let evidence = run_candidate_gate(request).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Passed);
        assert_eq!(evidence.base_commit, fixture.base);
        let rewrite = evidence.metadata_rewrite.unwrap();
        assert_eq!(rewrite.prior_gate_path, None);
        assert_eq!(
            rewrite.authorization,
            CandidateMetadataRewriteAuthorization::WriterCompletionRepair
        );
        assert_eq!(rewrite.rejected_candidate_commit, rejected);
        assert_eq!(rewrite.replacement_candidate_commit, replacement);
    }

    #[test]
    fn completion_repair_rewrite_still_rejects_tree_changes() {
        let fixture = Fixture::new();
        let rejected = fixture.commit("src/lib.rs", "pub fn value() {}\n", "Add value");
        fs::write(
            fixture.root.path().join("src/lib.rs"),
            "pub fn value() { 1 }\n",
        )
        .unwrap();
        crate::git::run(fixture.root.path(), &["add", "src/lib.rs"], "stage rewrite").unwrap();
        crate::git::run(
            fixture.root.path(),
            &["commit", "--amend", "-q", "-m", "Add value"],
            "rewrite candidate tree",
        )
        .unwrap();
        let replacement = crate::git::run_stdout(
            fixture.root.path(),
            &["rev-parse", "HEAD"],
            "read replacement",
        )
        .unwrap();
        let artifact = fixture.artifacts.path().join("replacement-gate.json");
        let mut request = fixture.request(&replacement, None, None, &artifact);
        request.base_commit = &rejected;
        request.allow_tree_identical_metadata_rewrite = true;

        let evidence = run_candidate_gate(request).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Blocked);
        assert!(
            check(&evidence, "candidate-identity")
                .diagnostics
                .iter()
                .any(|line| line.contains("does not match base tree"))
        );
    }

    #[test]
    fn accepts_overlong_commit_body_lines_as_project_policy() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "src/lib.rs",
            "pub fn value() {}\n",
            &format!("Add value\n\n- {}", "x".repeat(73)),
        );
        let artifact = fixture.artifacts.path().join("candidate-gate.json");

        let evidence =
            run_candidate_gate(fixture.request(&candidate, None, None, &artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Passed);
        assert_eq!(
            check(&evidence, "commit-metadata").status,
            CandidateCheckStatus::Passed
        );
    }

    #[test]
    fn accepts_subject_only_and_coauthored_commits_as_project_policy() {
        let subject_only_fixture = Fixture::new();
        let subject_only = subject_only_fixture.commit(
            "src/subject_only.rs",
            "pub fn subject_only() {}\n",
            "Add subject-only candidate",
        );
        let coauthored_fixture = Fixture::new();
        let coauthored = coauthored_fixture.commit(
            "src/coauthored.rs",
            "pub fn coauthored() {}\n",
            "Add coauthored candidate\n\nCo-Authored-By: Bot <bot@example.com>",
        );

        let subject_only_artifact = subject_only_fixture
            .artifacts
            .path()
            .join("subject-only.json");
        let subject_only_evidence = run_candidate_gate(subject_only_fixture.request(
            &subject_only,
            None,
            None,
            &subject_only_artifact,
        ))
        .unwrap();
        assert_eq!(
            check(&subject_only_evidence, "commit-metadata").status,
            CandidateCheckStatus::Passed
        );

        let coauthored_artifact = coauthored_fixture.artifacts.path().join("coauthored.json");
        let coauthored_evidence = run_candidate_gate(coauthored_fixture.request(
            &coauthored,
            None,
            None,
            &coauthored_artifact,
        ))
        .unwrap();
        assert_eq!(
            check(&coauthored_evidence, "commit-metadata").status,
            CandidateCheckStatus::Passed
        );
    }

    #[test]
    fn malformed_prior_gate_blocks_history_rewrite() {
        let fixture = Fixture::new();
        let rejected = fixture.commit("src/lib.rs", "pub fn value() {}\n", "Add bad value");
        crate::git::run(
            fixture.root.path(),
            &["commit", "--amend", "-q", "-m", "Add value"],
            "rewrite candidate",
        )
        .unwrap();
        let replacement = crate::git::run_stdout(
            fixture.root.path(),
            &["rev-parse", "HEAD"],
            "read replacement",
        )
        .unwrap();
        let prior_artifact = fixture.artifacts.path().join("prior-gate.json");
        fs::write(&prior_artifact, "{not-json\n").unwrap();
        let artifact = fixture.artifacts.path().join("replacement-gate.json");
        let mut request = fixture.request(&replacement, None, None, &artifact);
        request.base_commit = &rejected;

        let evidence = run_candidate_gate_with_prior(request, Some(&prior_artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Blocked);
        assert!(
            check(&evidence, "candidate-identity")
                .diagnostics
                .iter()
                .any(|line| line.contains("malformed"))
        );
    }

    #[test]
    fn continuation_rechecks_the_complete_candidate_range() {
        let fixture = Fixture::new();
        let rejected = fixture.commit(
            "src/lib.rs",
            "pub fn value() {    \n}\n",
            "Add value\n\n- Explain the value",
        );
        let prior_artifact = fixture.artifacts.path().join("prior-gate.json");
        run_candidate_gate(fixture.request(&rejected, None, None, &prior_artifact)).unwrap();
        let replacement = fixture.commit(
            "src/lib.rs",
            "pub fn value() {}\n",
            "Clean value\n\n- Remove the whitespace error",
        );
        let artifact = fixture.artifacts.path().join("replacement-gate.json");
        let mut request = fixture.request(&replacement, None, None, &artifact);
        request.base_commit = &rejected;

        let evidence = run_candidate_gate_with_prior(request, Some(&prior_artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Passed);
        assert_eq!(evidence.base_commit, fixture.base);
        assert!(evidence.metadata_rewrite.is_none());
    }

    #[test]
    fn appended_commit_cannot_hide_prior_metadata_failure() {
        let fixture = Fixture::new();
        crate::git::run(
            fixture.root.path(),
            &["config", "user.email", "writer@example.invalid"],
            "set invalid ancestor identity",
        )
        .unwrap();
        let rejected = fixture.commit(
            "src/lib.rs",
            "pub fn value() {}\n",
            "Add value\\n\\nExplain value",
        );
        crate::git::run(
            fixture.root.path(),
            &["config", "user.email", "test@example.com"],
            "restore valid identity",
        )
        .unwrap();
        let prior_artifact = fixture.artifacts.path().join("prior-gate.json");
        run_candidate_gate(fixture.request(&rejected, None, None, &prior_artifact)).unwrap();
        let replacement = fixture.commit("src/extra.rs", "pub fn extra() {}\n", "Add extra");
        let artifact = fixture.artifacts.path().join("replacement-gate.json");
        let mut request = fixture.request(&replacement, None, None, &artifact);
        request.base_commit = &rejected;

        let evidence = run_candidate_gate_with_prior(request, Some(&prior_artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Rejected);
        assert_eq!(evidence.base_commit, fixture.base);
        assert_eq!(
            check(&evidence, "commit-metadata").status,
            CandidateCheckStatus::Failed
        );
    }

    #[test]
    fn mismatched_candidate_head_blocks_with_persisted_identity_evidence() {
        let fixture = Fixture::new();
        let candidate = fixture.commit("src/lib.rs", "pub fn value() {}\n", "Add value");
        let artifact = fixture.artifacts.path().join("candidate-gate.json");

        let evidence =
            run_candidate_gate(fixture.request(&fixture.base, None, None, &artifact)).unwrap();

        assert_ne!(candidate, fixture.base);
        assert_eq!(evidence.disposition, CandidateGateDisposition::Blocked);
        let identity = check(&evidence, "candidate-identity");
        assert_eq!(identity.status, CandidateCheckStatus::Error);
        assert!(
            identity
                .diagnostics
                .iter()
                .any(|line| line.contains("HEAD"))
        );
        assert!(artifact.is_file());
    }

    #[test]
    fn dirty_candidate_workspace_blocks_before_review() {
        let fixture = Fixture::new();
        let candidate = fixture.commit("src/lib.rs", "pub fn value() {}\n", "Add value");
        fs::write(fixture.root.path().join("untracked.txt"), "dirty\n").unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");

        let evidence =
            run_candidate_gate(fixture.request(&candidate, None, None, &artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Blocked);
        let identity = check(&evidence, "candidate-identity");
        assert_eq!(identity.status, CandidateCheckStatus::Error);
        assert!(
            identity
                .diagnostics
                .iter()
                .any(|line| line.contains("dirty"))
        );
    }

    #[test]
    fn rejects_missing_test_paths_and_selectors_without_aliases() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/dashboard.rs",
            "#[test]\nfn another_test() {}\n",
            "Add dashboard test\n\n- Cover dashboard selection",
        );
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract("tests/dashboard.rs (preserves_selection)");

        let evidence =
            run_candidate_gate(fixture.request(&candidate, Some(&contract), None, &artifact))
                .unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Rejected);
        assert!(
            check(&evidence, "behavior-references")
                .diagnostics
                .iter()
                .any(|line| line.contains("preserves_selection"))
        );
    }

    #[test]
    fn validates_test_references_against_passing_structured_tester_evidence() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/dashboard.rs",
            "#[test]\nfn preserves_selection() {}\n",
            "Add dashboard test\n\n- Cover dashboard selection",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        fs::write(
            &tester,
            format!(
                "{{\"candidate_commit\":\"{candidate}\",\"commands\":[],\"tests\":[{{\"id\":\"dashboard::preserves_selection\",\"test_harness\":\"cargo-nextest\",\"status\":\"pass\",\"duration_ms\":1,\"failure_excerpt\":null}}],\"summary\":{{\"total\":1,\"pass\":1,\"fail\":0,\"skipped\":0}},\"error\":null}}"
            ),
        )
        .unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract("tests/dashboard.rs (preserves_selection)");

        let evidence = run_candidate_gate(fixture.request(
            &candidate,
            Some(&contract),
            Some(&tester),
            &artifact,
        ))
        .unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Passed);
        assert_eq!(
            check(&evidence, "tester-reference-evidence").status,
            CandidateCheckStatus::Passed
        );
    }

    #[test]
    fn accepts_class_qualified_python_unittest_reference() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/test_ticket_queue.py",
            "import unittest\n\nclass TicketQueueTests(unittest.TestCase):\n    def test_recovery_is_idempotent(self):\n        pass\n",
            "Add queue test\n\n- Cover recovery",
        );
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        for reference in [
            "tests/test_ticket_queue.py::TicketQueueTests.test_recovery_is_idempotent",
            "tests/test_ticket_queue.py (TicketQueueTests.test_recovery_is_idempotent)",
            "tests/test_ticket_queue.py::test_recovery_is_idempotent",
        ] {
            let contract = behavior_contract(reference);
            let evidence =
                run_candidate_gate(fixture.request(&candidate, Some(&contract), None, &artifact))
                    .unwrap();
            assert_eq!(
                evidence.disposition,
                CandidateGateDisposition::Passed,
                "{reference}: {evidence:#?}"
            );
            assert_eq!(
                check(&evidence, "tester-reference-evidence").status,
                CandidateCheckStatus::Skipped
            );
        }
    }

    #[test]
    fn resolves_python_declarations_without_executing_source() {
        let reference = parse_test_reference(
            "tests/test_ticket_queue.py::TicketQueueTests.test_recovery_is_idempotent",
        )
        .unwrap();
        for source in [
            "raise RuntimeError('do not execute candidate code')\nclass TicketQueueTests:\n    def test_recovery_is_idempotent(self): pass\n",
            "@decorate('class Fake:')\nclass TicketQueueTests(\n    unittest.TestCase,\n):\n    @decorate(\n        'def fake():',\n    )\n    def test_recovery_is_idempotent(\n        self,\n    ):\n        pass\n",
            "class TicketQueueTests:\r\n\tasync def test_recovery_is_idempotent(self): pass\r\n",
            "class TicketQueueTests:\n    def helper(self):\n        pass\n\n    # ignore a comment with unmatched delimiters: [ '\n    def test_recovery_is_idempotent(self):\n        text = '\\\"'\n",
        ] {
            assert!(reference.source_contains_selector(source), "{source}");
        }
        for source in [
            "class TicketQueueTests:\n    def test_recovery_is_idempotent_suffix(self): pass\n",
            "class TicketQueueTests:\n    def test_recovery_is_idempotent(self): pass\nclass TicketQueueTests:\n    pass\n",
            "class TicketQueueTests:\n    if False:\n        def test_recovery_is_idempotent(self): pass\n",
            "class TicketQueueTests:\n    text = '''unterminated\n    def test_recovery_is_idempotent(self): pass\n",
            "class TicketQueueTests:\n    text = [)\n    def test_recovery_is_idempotent(self): pass\n",
            "text = (\n    '''class TicketQueueTests:\n    def test_recovery_is_idempotent(self): pass\n'''\n)\n",
        ] {
            assert!(!reference.source_contains_selector(source), "{source}");
        }
        for selector in [
            "TicketQueueTests..test_recovery_is_idempotent",
            "TicketQueueTests.test_recovery_is_idempotent()",
            "1Class.test_recovery_is_idempotent",
            "TicketQueueTests::test_recovery_is_idempotent",
        ] {
            let reference =
                parse_test_reference(&format!("tests/test_ticket_queue.py ({selector})")).unwrap();
            assert!(
                !reference.source_contains_selector(&format!("# {selector}\n")),
                "{selector}"
            );
        }
    }

    #[test]
    fn rejects_python_method_outside_referenced_class() {
        for source in [
            "class TicketQueueTests:\n    pass\n\nclass OtherTests:\n    def test_recovery_is_idempotent(self): pass\n",
            "class TicketQueueTests:\n    pass\n\ndef test_recovery_is_idempotent(): pass\n",
            "class TicketQueueTests:\n    def helper(self):\n        def test_recovery_is_idempotent(): pass\n",
            "class TicketQueueTests:\n    class Nested:\n        def test_recovery_is_idempotent(self): pass\n",
            "# TicketQueueTests.test_recovery_is_idempotent\nclass TicketQueueTests:\n    pass\n",
            "text = '''\nclass TicketQueueTests:\n    def test_recovery_is_idempotent(self): pass\n'''\n",
            "class TicketQueueTests:\n    text = '''\n    def test_recovery_is_idempotent(self): pass\n    '''\n",
        ] {
            let fixture = Fixture::new();
            let candidate = fixture.commit(
                "tests/test_ticket_queue.py",
                source,
                "Add queue fixture\n\n- Cover source ownership",
            );
            let artifact = fixture.artifacts.path().join("candidate-gate.json");
            let contract = behavior_contract(
                "tests/test_ticket_queue.py::TicketQueueTests.test_recovery_is_idempotent",
            );
            let evidence =
                run_candidate_gate(fixture.request(&candidate, Some(&contract), None, &artifact))
                    .unwrap();
            assert_eq!(
                check(&evidence, "behavior-references").status,
                CandidateCheckStatus::Failed,
                "{source}: {evidence:#?}"
            );
            assert_eq!(evidence.disposition, CandidateGateDisposition::Rejected);
        }
    }

    #[test]
    fn accepts_class_qualified_python_unittest_evidence() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/test_ticket_queue.py",
            "class TicketQueueTests:\n    def test_recovery_is_idempotent(self): pass\n",
            "Add queue test\n\n- Cover recovery",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract(
            "tests/test_ticket_queue.py::TicketQueueTests.test_recovery_is_idempotent",
        );
        for identity in [
            "TicketQueueTests.test_recovery_is_idempotent",
            "tests.test_ticket_queue.TicketQueueTests.test_recovery_is_idempotent",
            "test_recovery_is_idempotent (tests.test_ticket_queue.TicketQueueTests)",
            "test_recovery_is_idempotent (tests.test_ticket_queue.TicketQueueTests.test_recovery_is_idempotent)",
            "tests/test_ticket_queue.py::TicketQueueTests.test_recovery_is_idempotent",
            "tests/test_ticket_queue.py (TicketQueueTests.test_recovery_is_idempotent)",
        ] {
            let results = serde_json::json!({
                "candidate_commit": candidate, "commands": [],
                "tests": [{"id": identity, "test_harness": "unittest", "status": "pass"}],
                "summary": {"total": 1, "pass": 1, "fail": 0, "skipped": 0}, "error": null
            });
            fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
            let mut request =
                fixture.request(&candidate, Some(&contract), Some(&tester), &artifact);
            request.phase = CandidateGatePhase::FinalTester;
            let evidence = run_candidate_gate(request).unwrap();
            assert_eq!(
                evidence.disposition,
                CandidateGateDisposition::Passed,
                "{identity}: {evidence:#?}"
            );
            let preserved: serde_json::Value =
                serde_json::from_slice(&fs::read(&tester).unwrap()).unwrap();
            assert_eq!(preserved, results);
        }
    }

    #[test]
    fn resolves_short_python_evidence_without_bridging_classes() {
        let reference =
            parse_test_reference("tests/test_ticket_queue.py::test_recovery_is_idempotent")
                .unwrap();
        let qualified_reference = parse_test_reference(
            "tests/test_ticket_queue.py::TicketQueueTests.test_recovery_is_idempotent",
        )
        .unwrap();
        let cases: &[(&[(&str, &str)], bool, bool)] = &[
            (&[("test_recovery_is_idempotent", "pass")], true, true),
            (
                &[("TicketQueueTests.test_recovery_is_idempotent", "pass")],
                true,
                true,
            ),
            (
                &[
                    ("TicketQueueTests.test_recovery_is_idempotent", "pass"),
                    ("OtherTests.test_recovery_is_idempotent", "fail"),
                ],
                false,
                true,
            ),
            (
                &[
                    ("TicketQueueTests.test_recovery_is_idempotent", "pass"),
                    ("OtherTests.test_recovery_is_idempotent", "pass"),
                ],
                false,
                true,
            ),
            (
                &[
                    ("test_recovery_is_idempotent", "pass"),
                    ("OtherTests.test_recovery_is_idempotent", "pass"),
                ],
                true,
                false,
            ),
            (
                &[
                    ("test_recovery_is_idempotent", "pass"),
                    ("OtherTests.test_recovery_is_idempotent", "skipped"),
                ],
                false,
                false,
            ),
        ];
        for (identities, short_passes, qualified_passes) in cases {
            let mut tests = identities
                .iter()
                .map(|(id, status)| {
                    serde_json::from_value::<crate::tester::TestResult>(
                        serde_json::json!({"id": id, "test_harness": "unittest", "status": status}),
                    )
                    .unwrap()
                })
                .collect::<Vec<_>>();
            for _ in 0..2 {
                let parsed = tests
                    .iter()
                    .map(|test| (PythonTestIdentity::parse(&test.id).unwrap(), test))
                    .collect::<Vec<_>>();
                assert_eq!(
                    python_evidence_matches_reference(
                        &reference.python_identity().unwrap(),
                        &parsed
                    ),
                    *short_passes,
                    "{identities:?}"
                );
                assert_eq!(
                    python_evidence_matches_reference(
                        &qualified_reference.python_identity().unwrap(),
                        &parsed
                    ),
                    *qualified_passes,
                    "{identities:?}"
                );
                tests.reverse();
            }
        }
    }

    #[test]
    fn rejects_conflicting_class_qualified_python_evidence() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/test_ticket_queue.py",
            "class TicketQueueTests:\n    def test_recovery_is_idempotent(self): pass\n",
            "Add queue test\n\n- Cover recovery",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract(
            "tests/test_ticket_queue.py::TicketQueueTests.test_recovery_is_idempotent",
        );
        let cases: &[&[(&str, &str)]] = &[
            &[],
            &[("OtherTests.test_recovery_is_idempotent", "pass")],
            &[("TicketQueueTests.test_missing", "pass")],
            &[("TicketQueueTests.test_recovery_is_idempotent", "fail")],
            &[("TicketQueueTests.test_recovery_is_idempotent", "skipped")],
            &[(
                "tests/other.py::TicketQueueTests.test_recovery_is_idempotent",
                "pass",
            )],
            &[
                ("test_recovery_is_idempotent", "pass"),
                ("OtherTests.test_recovery_is_idempotent", "fail"),
            ],
            &[
                ("TicketQueueTests.test_recovery_is_idempotent", "pass"),
                (
                    "test_recovery_is_idempotent (tests.test_ticket_queue.TicketQueueTests)",
                    "skipped",
                ),
            ],
            &[
                ("one.TicketQueueTests.test_recovery_is_idempotent", "pass"),
                ("two.TicketQueueTests.test_recovery_is_idempotent", "pass"),
            ],
            &[(
                "arbitrary text TicketQueueTests.test_recovery_is_idempotent",
                "pass",
            )],
            &[(
                "test_other (tests.test_ticket_queue.TicketQueueTests.test_recovery_is_idempotent)",
                "pass",
            )],
        ];
        for identities in cases {
            let tests = identities.iter().map(|(id, status)| serde_json::json!({"id": id, "test_harness": "unittest", "status": status})).collect::<Vec<_>>();
            let results = serde_json::json!({
                "candidate_commit": candidate, "commands": [], "tests": tests,
                "summary": {"total": tests.len(), "pass": identities.iter().filter(|(_, status)| *status == "pass").count(), "fail": identities.iter().filter(|(_, status)| *status == "fail").count(), "skipped": identities.iter().filter(|(_, status)| *status == "skipped").count()}, "error": null
            });
            fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
            let mut request =
                fixture.request(&candidate, Some(&contract), Some(&tester), &artifact);
            request.phase = CandidateGatePhase::FinalTester;
            let evidence = run_candidate_gate(request).unwrap();
            assert_eq!(
                check(&evidence, "tester-reference-evidence").status,
                CandidateCheckStatus::Failed,
                "{identities:?}: {evidence:#?}"
            );
            assert_eq!(evidence.disposition, CandidateGateDisposition::Rejected);
        }
    }

    #[test]
    fn accepts_equivalent_native_rust_identities() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "src/lib.rs",
            "#[cfg(test)] mod tests { mod lifecycle { #[test] fn drain_is_idempotent() {} } }\n",
            "Add drain lifecycle\n\n- Cover idempotent drain",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        for (selector, identity, accepted) in [
            (
                "drain_is_idempotent",
                "my-crate::bin/tool drain_is_idempotent",
                true,
            ),
            (
                "drain_is_idempotent",
                "my-crate::bin/01-tool$tests::lifecycle::drain_is_idempotent",
                true,
            ),
            (
                "tests::lifecycle::drain_is_idempotent",
                "my-crate::bench/lifecycle tests::lifecycle::drain_is_idempotent",
                true,
            ),
            (
                "my-crate::example/tool$tests::lifecycle::drain_is_idempotent",
                "src/lib.rs (my-crate::example/tool tests::lifecycle::drain_is_idempotent)",
                true,
            ),
            (
                "my-crate::bin/tool$tests::lifecycle::drain_is_idempotent",
                "my-crate::example/tool tests::lifecycle::drain_is_idempotent",
                false,
            ),
            (
                "my-crate::bin/tool$tests::lifecycle::drain_is_idempotent",
                "my-crate::bin/other tests::lifecycle::drain_is_idempotent",
                false,
            ),
            (
                "drain_is_idempotent",
                "my-crate::bin/ drain_is_idempotent",
                false,
            ),
            (
                "drain_is_idempotent",
                "my-crate::/tool drain_is_idempotent",
                false,
            ),
            (
                "drain_is_idempotent",
                "::bin/tool drain_is_idempotent",
                false,
            ),
            (
                "drain_is_idempotent",
                "my-crate/bin/tool drain_is_idempotent",
                false,
            ),
            (
                "drain_is_idempotent",
                "my-crate::bin/tools/tool drain_is_idempotent",
                false,
            ),
            (
                "drain_is_idempotent",
                "my-crate::bin/tool::nested drain_is_idempotent",
                false,
            ),
            (
                "drain_is_idempotent",
                "my-crate::bin/tool tests/lifecycle/drain_is_idempotent",
                false,
            ),
            (
                "drain_is_idempotent",
                "src/lib.rs (tests::lifecycle::drain_is_idempotent)",
                true,
            ),
            ("drain_is_idempotent", "drain_is_idempotent", true),
            (
                "drain_is_idempotent",
                "tests::lifecycle::drain_is_idempotent",
                true,
            ),
            (
                "drain_is_idempotent",
                "src/lib.rs (drain_is_idempotent)",
                true,
            ),
            (
                "drain_is_idempotent",
                "src/lib.rs::tests::lifecycle::drain_is_idempotent",
                true,
            ),
            (
                "drain_is_idempotent",
                "fluent::lib$tests::lifecycle::drain_is_idempotent",
                true,
            ),
            (
                "drain_is_idempotent",
                "fluent::lib tests::lifecycle::drain_is_idempotent",
                true,
            ),
            (
                "tests::lifecycle::drain_is_idempotent",
                "src/lib.rs (tests::lifecycle::drain_is_idempotent)",
                true,
            ),
            (
                "tests::lifecycle::drain_is_idempotent",
                "fluent::lib$tests::lifecycle::drain_is_idempotent",
                true,
            ),
            (
                "fluent::lib$tests::lifecycle::drain_is_idempotent",
                "fluent::lib tests::lifecycle::drain_is_idempotent",
                true,
            ),
            (
                "drain_is_idempotent",
                "src/other.rs (tests::lifecycle::drain_is_idempotent)",
                false,
            ),
            (
                "drain_is_idempotent",
                "src/other.rs::drain_is_idempotent",
                false,
            ),
            (
                "tests::lifecycle::drain_is_idempotent",
                "other::lifecycle::drain_is_idempotent",
                false,
            ),
            (
                "lifecycle::drain_is_idempotent",
                "tests::lifecycle::drain_is_idempotent",
                false,
            ),
            (
                "fluent::lib$tests::lifecycle::drain_is_idempotent",
                "other::lib tests::lifecycle::drain_is_idempotent",
                false,
            ),
            (
                "drain_is_idempotent",
                "arbitrary display text drain_is_idempotent",
                false,
            ),
            (
                "drain_is_idempotent",
                "unrelated_drain_is_idempotent",
                false,
            ),
        ] {
            let results = serde_json::json!({
                "candidate_commit": candidate,
                "commands": [],
                "tests": [{"id": identity, "test_harness": "cargo-nextest", "status": "pass"}],
                "summary": {"total": 1, "pass": 1, "fail": 0, "skipped": 0},
                "error": null
            });
            fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
            let contract = behavior_contract(&format!("src/lib.rs ({selector})"));
            let mut request =
                fixture.request(&candidate, Some(&contract), Some(&tester), &artifact);
            request.phase = CandidateGatePhase::FinalTester;
            let evidence = run_candidate_gate(request).unwrap();

            assert_eq!(
                check(&evidence, "behavior-references").status,
                CandidateCheckStatus::Passed,
                "source reference {selector}: {evidence:#?}"
            );
            assert_eq!(
                evidence.disposition,
                if accepted {
                    CandidateGateDisposition::Passed
                } else {
                    CandidateGateDisposition::Rejected
                },
                "reference {selector}, result {identity}: {evidence:#?}"
            );
            let preserved: serde_json::Value =
                serde_json::from_slice(&fs::read(&tester).unwrap()).unwrap();
            assert_eq!(preserved, results);
        }
    }

    #[test]
    fn rejects_invalid_or_ambiguous_test_evidence() {
        let fixture = Fixture::new();
        fixture.commit(
            "tests/lifecycle.sh",
            "#!/bin/sh\nexit 0\n",
            "Add lifecycle check\n\n- Exercise lifecycle commands",
        );
        let candidate = fixture.commit(
            "src/lib.rs",
            "#[cfg(test)] mod tests { mod a { #[test] fn works() {} } mod b { #[test] fn works() {} } }\n",
            "Add qualified tests\n\n- Cover module distinctions",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let cases: &[(&str, &[(&str, &str)], bool)] = &[
            (
                "src/lib.rs (works)",
                &[
                    ("my-crate works", "pass"),
                    ("my-crate::bin/tool works", "skipped"),
                ],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[
                    ("my-crate works", "pass"),
                    ("my-crate::bin/tool$works", "fail"),
                ],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[
                    ("my-crate::bin/tool works", "pass"),
                    ("my-crate::bin/other$works", "skipped"),
                ],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[
                    ("my-crate::bin/tool works", "pass"),
                    ("my-crate::example/tool$works", "fail"),
                ],
                false,
            ),
            (
                "src/lib.rs (my-crate::bin/tool$works)",
                &[
                    ("my-crate::bin/tool works", "pass"),
                    ("my-crate::example/tool$works", "skipped"),
                ],
                true,
            ),
            (
                "src/lib.rs (works)",
                &[("tests::a::works", "pass"), ("tests::b::works", "fail")],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[("tests::a::works", "pass"), ("tests::b::works", "skipped")],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[("tests::a::works", "pass"), ("tests::b::works", "pass")],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[
                    ("one$tests::a::works", "pass"),
                    ("two tests::a::works", "fail"),
                ],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[
                    ("one$tests::a::works", "pass"),
                    ("two tests::a::works", "skipped"),
                ],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[
                    ("src/lib.rs (tests::a::works)", "pass"),
                    ("src/lib.rs (tests::b::works)", "fail"),
                ],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[("tests::a::works", "pass"), ("tests::a::works", "fail")],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[
                    ("tests::a::works", "pass"),
                    ("src/lib.rs (tests::a::works)", "skipped"),
                ],
                false,
            ),
            (
                "src/lib.rs (tests::a::works)",
                &[("works", "pass"), ("tests::b::works", "fail")],
                false,
            ),
            ("src/lib.rs (works)", &[], false),
            ("src/lib.rs (works)", &[("tests::a::works", "fail")], false),
            (
                "src/lib.rs (works)",
                &[("tests::a::works", "skipped")],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[("tests::a::works", "not_run")],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[("tests::a::unrelated", "pass")],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[("src/other.rs (works)", "pass")],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[("src/lib.rs (works", "pass")],
                false,
            ),
            (
                "src/lib.rs (works)",
                &[("arbitrary prose works", "pass")],
                false,
            ),
            ("src/lib.rs (works)", &[("tests::a::works", "pass")], true),
            (
                "src/lib.rs (works)",
                &[
                    ("tests::a::works", "pass"),
                    ("src/lib.rs (tests::a::works)", "pass"),
                ],
                true,
            ),
            (
                "src/lib.rs (tests::a::works)",
                &[("tests::a::works", "pass"), ("tests::b::works", "fail")],
                true,
            ),
            (
                "src/lib.rs (one$tests::a::works)",
                &[
                    ("one tests::a::works", "pass"),
                    ("two$tests::a::works", "skipped"),
                ],
                true,
            ),
            (
                "src/lib.rs (works)",
                &[
                    ("src/lib.rs (tests::a::works)", "pass"),
                    ("src/other.rs (tests::a::works)", "fail"),
                ],
                true,
            ),
            (
                "tests/lifecycle.sh (covers lifecycle)",
                &[
                    ("tests/lifecycle.sh", "pass"),
                    ("tests/lifecycle.sh", "fail"),
                ],
                false,
            ),
        ];
        for (reference, rows, accepted) in cases {
            // Evidence order must never decide which same-named test wins.
            for reverse in [false, true] {
                let mut tests = rows
                    .iter()
                    .map(|(id, status)| {
                        serde_json::json!({
                            "id": id, "test_harness": "project checks", "status": status
                        })
                    })
                    .collect::<Vec<_>>();
                if reverse {
                    tests.reverse();
                }
                let results = serde_json::json!({
                    "candidate_commit": candidate,
                    "commands": [],
                    "tests": tests,
                    "summary": {
                        "total": rows.len(),
                        "pass": rows.iter().filter(|(_, status)| *status == "pass").count(),
                        "fail": rows.iter().filter(|(_, status)| *status == "fail").count(),
                        "skipped": rows.iter().filter(|(_, status)| *status == "skipped").count()
                    },
                    "error": null
                });
                fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
                let contract = behavior_contract(reference);
                let mut request =
                    fixture.request(&candidate, Some(&contract), Some(&tester), &artifact);
                request.phase = CandidateGatePhase::FinalTester;
                let evidence = run_candidate_gate(request).unwrap();
                assert_eq!(
                    check(&evidence, "behavior-references").status,
                    CandidateCheckStatus::Passed
                );
                let tester_check = check(&evidence, "tester-reference-evidence");
                assert_eq!(
                    tester_check.status,
                    if *accepted {
                        CandidateCheckStatus::Passed
                    } else {
                        CandidateCheckStatus::Failed
                    },
                    "{reference}, {rows:?}, reversed={reverse}: {evidence:#?}"
                );
                assert_eq!(
                    evidence.disposition,
                    if *accepted {
                        CandidateGateDisposition::Passed
                    } else {
                        CandidateGateDisposition::Rejected
                    }
                );
                if !accepted {
                    assert!(
                        tester_check
                            .diagnostics
                            .iter()
                            .any(|line| line.contains(reference)),
                        "{tester_check:#?}"
                    );
                    for (id, _) in *rows {
                        assert!(
                            tester_check
                                .diagnostics
                                .iter()
                                .any(|line| line.contains(id)),
                            "missing observed {id}: {tester_check:#?}"
                        );
                    }
                }
                let preserved: serde_json::Value =
                    serde_json::from_slice(&fs::read(&tester).unwrap()).unwrap();
                assert_eq!(preserved, results);
            }
        }

        for (reference, id) in [
            ("src/lib.rs (works)", "tests::a::works"),
            (
                "tests/lifecycle.sh (covers lifecycle)",
                "tests/lifecycle.sh",
            ),
        ] {
            for commit in [Some(candidate.as_str()), Some(fixture.base.as_str()), None] {
                let results = serde_json::json!({
                    "candidate_commit": commit,
                    "commands": [],
                    "tests": [{"id": id, "test_harness": "project checks", "status": "pass"}],
                    "summary": {"total": 1, "pass": 1, "fail": 0, "skipped": 0},
                    "error": null
                });
                fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
                let contract = behavior_contract(reference);
                let mut request =
                    fixture.request(&candidate, Some(&contract), Some(&tester), &artifact);
                request.phase = CandidateGatePhase::FinalTester;
                let evidence = run_candidate_gate(request).unwrap();
                let bound = commit == Some(candidate.as_str());
                assert_eq!(
                    check(&evidence, "tester-reference-evidence").status,
                    if bound {
                        CandidateCheckStatus::Passed
                    } else {
                        CandidateCheckStatus::Error
                    },
                    "{evidence:#?}"
                );
                assert_eq!(
                    evidence.disposition,
                    if bound {
                        CandidateGateDisposition::Passed
                    } else {
                        CandidateGateDisposition::Blocked
                    }
                );
            }
        }
    }

    #[test]
    fn validates_parenthesized_source_path_test_identity() {
        let fixture = Fixture::new();
        let selector = "drain_is_idempotent";
        let candidate = fixture.commit(
            "src/lib.rs",
            &format!("#[test]\nfn {selector}() {{}}\n"),
            "Add drain lifecycle\n\n- Cover idempotent drain",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        let results = serde_json::json!({
            "candidate_commit": candidate,
            "commands": [],
            "tests": [{
                "id": format!("src/lib.rs ({selector})"),
                "test_harness": "cargo-nextest",
                "status": "pass",
                "duration_ms": 1,
                "failure_excerpt": null
            }],
            "summary": {"total": 1, "pass": 1, "fail": 0, "skipped": 0},
            "error": null
        });
        fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract(&format!("src/lib.rs ({selector})"));

        let evidence = run_candidate_gate(fixture.request(
            &candidate,
            Some(&contract),
            Some(&tester),
            &artifact,
        ))
        .unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Passed);
        assert_eq!(
            check(&evidence, "tester-reference-evidence").status,
            CandidateCheckStatus::Passed
        );
    }

    #[test]
    fn python_command_reference_matches_exact_passing_tester_command() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/fluent_merge_readiness.py",
            "#!/usr/bin/env python3\n",
            "Add readiness journey\n\n- Cover the public workflow",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        let results = serde_json::json!({
            "candidate_commit": candidate,
            "commands": [{
                "command": "python3 tests/fluent_merge_readiness.py",
                "test_harness": "shell-harness",
                "exit_code": 0,
                "duration_ms": 1,
                "stdout_log": "commands/0-stdout.log",
                "stderr_log": "commands/0-stderr.log"
            }],
            "tests": [],
            "summary": {"total": 0, "pass": 0, "fail": 0, "skipped": 0},
            "error": null
        });
        fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract("python3 tests/fluent_merge_readiness.py");

        let evidence = run_candidate_gate(fixture.request(
            &candidate,
            Some(&contract),
            Some(&tester),
            &artifact,
        ))
        .unwrap();

        assert_eq!(
            evidence.disposition,
            CandidateGateDisposition::Passed,
            "{evidence:#?}"
        );
        assert_eq!(
            check(&evidence, "tester-reference-evidence").status,
            CandidateCheckStatus::Passed
        );
    }

    #[test]
    fn python_command_reference_matches_accepted_candidate_bound_host_evidence() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/fluent_merge_readiness.py",
            "#!/usr/bin/env python3\n",
            "Add readiness journey\n\n- Cover the public workflow",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        let results = serde_json::json!({
            "candidate_commit": candidate,
            "commands": [{
                "command": "python3 tests/fluent_merge_readiness.py",
                "test_harness": "shell-harness",
                "exit_code": 1,
                "duration_ms": 1,
                "stdout_log": "commands/0-stdout.log",
                "stderr_log": "commands/0-stderr.log"
            }],
            "tests": [],
            "summary": {"total": 0, "pass": 0, "fail": 0, "skipped": 0},
            "error": null
        });
        fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract("python3 tests/fluent_merge_readiness.py");
        let accepted = vec!["python3 tests/fluent_merge_readiness.py".to_string()];
        let mut request = fixture.request(&candidate, Some(&contract), Some(&tester), &artifact);
        request.accepted_tester_commands = &accepted;

        let evidence = run_candidate_gate(request).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Passed);
        assert_eq!(
            check(&evidence, "tester-reference-evidence").status,
            CandidateCheckStatus::Passed
        );
    }

    #[test]
    fn nextest_binary_and_test_identity_matches_exact_selector() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/dashboard.rs",
            "#[test]\nfn preserves_selection() {}\n",
            "Add dashboard test\n\n- Cover dashboard selection",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        let results = serde_json::json!({
            "candidate_commit": candidate,
            "commands": [],
            "tests": [{
                "id": "dashboard-tests::dashboard preserves_selection",
                "test_harness": "cargo-nextest",
                "status": "pass",
                "duration_ms": 1,
                "failure_excerpt": null
            }],
            "summary": {"total": 1, "pass": 1, "fail": 0, "skipped": 0},
            "error": null
        });
        fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract("tests/dashboard.rs (preserves_selection)");

        let evidence = run_candidate_gate(fixture.request(
            &candidate,
            Some(&contract),
            Some(&tester),
            &artifact,
        ))
        .unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Passed);
    }

    #[test]
    fn python_command_reference_rejects_different_passing_tester_command() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/fluent_merge_readiness.py",
            "#!/usr/bin/env python3\n",
            "Add readiness journey\n\n- Cover the public workflow",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        let results = serde_json::json!({
            "candidate_commit": candidate,
            "commands": [{
                "command": "python3 tests/a_different_journey.py",
                "test_harness": "shell-harness",
                "exit_code": 0,
                "duration_ms": 1,
                "stdout_log": "commands/0-stdout.log",
                "stderr_log": "commands/0-stderr.log"
            }],
            "tests": [],
            "summary": {"total": 0, "pass": 0, "fail": 0, "skipped": 0},
            "error": null
        });
        fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract("python3 tests/fluent_merge_readiness.py");

        let evidence = run_candidate_gate(fixture.request(
            &candidate,
            Some(&contract),
            Some(&tester),
            &artifact,
        ))
        .unwrap();

        assert_eq!(
            check(&evidence, "tester-reference-evidence").diagnostics,
            vec![
                "no passing Tester result matches python3 tests/fluent_merge_readiness.py"
                    .to_string()
            ]
        );
    }

    #[test]
    fn missing_test_reference_reports_failed_tester_command_evidence() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/dashboard.rs",
            "#[test]\nfn preserves_selection() {}\n",
            "Add dashboard test",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        let results = serde_json::json!({
            "candidate_commit": candidate,
            "commands": [{
                "command": "cargo nextest run --test dashboard",
                "test_harness": "cargo-nextest",
                "exit_code": 101,
                "duration_ms": 1,
                "stdout_log": "commands/0-stdout.log",
                "stderr_log": "commands/0-stderr.log"
            }],
            "tests": [],
            "summary": {"total": 0, "pass": 0, "fail": 0, "skipped": 0},
            "error": null
        });
        fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract("tests/dashboard.rs (preserves_selection)");

        let evidence = run_candidate_gate(fixture.request(
            &candidate,
            Some(&contract),
            Some(&tester),
            &artifact,
        ))
        .unwrap();

        let tester_check = check(&evidence, "tester-reference-evidence");
        assert_eq!(tester_check.status, CandidateCheckStatus::Failed);
        assert_eq!(
            tester_check.diagnostics,
            vec![
                "Tester command 0 exited 101: cargo nextest run --test dashboard".to_string(),
                format!(
                    "Tester command 0 stdout log: {}",
                    fixture
                        .artifacts
                        .path()
                        .join("commands/0-stdout.log")
                        .display()
                ),
                format!(
                    "Tester command 0 stderr log: {}",
                    fixture
                        .artifacts
                        .path()
                        .join("commands/0-stderr.log")
                        .display()
                ),
                "no passing Tester result matches tests/dashboard.rs (preserves_selection)".to_string(),
                "observed Tester identities for tests/dashboard.rs (preserves_selection) (showing 0 of 0): ".to_string(),
            ]
        );
    }

    #[test]
    fn validates_documented_double_colon_test_references() {
        let fixture = Fixture::new();
        let selector = "concurrent_registry_mutations_preserve_every_successful_change";
        let candidate = fixture.commit(
            "tests/scheduler_service.rs",
            &format!("#[test]\nfn {selector}() {{}}\n"),
            "Test concurrent registry unregistration\n\n- Cover registry updates",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        let results = serde_json::json!({
            "candidate_commit": candidate,
            "commands": [],
            "tests": [{
                "id": format!("fluent::scheduler_service${selector}"),
                "test_harness": "cargo-nextest",
                "status": "pass",
                "duration_ms": 1,
                "failure_excerpt": null
            }],
            "summary": {"total": 1, "pass": 1, "fail": 0, "skipped": 0},
            "error": null
        });
        fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract(&format!("tests/scheduler_service.rs::{selector}"));

        let evidence = run_candidate_gate(fixture.request(
            &candidate,
            Some(&contract),
            Some(&tester),
            &artifact,
        ))
        .unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Passed);
        assert_eq!(
            check(&evidence, "behavior-references").status,
            CandidateCheckStatus::Passed
        );
        assert_eq!(
            check(&evidence, "tester-reference-evidence").status,
            CandidateCheckStatus::Passed
        );
    }

    #[test]
    fn validates_nextest_integration_test_references() {
        let fixture = Fixture::new();
        let selector = "skills_add_replaces_stale_shim_installation";
        let candidate = fixture.commit(
            "tests/binary.rs",
            &format!("#[test]\nfn {selector}() {{}}\n"),
            "Add skills test\n\n- Cover skill installation",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        let results = serde_json::json!({
            "candidate_commit": candidate,
            "commands": [],
            "tests": [{
                "id": format!("fluent::binary${selector}"),
                "test_harness": "cargo-nextest",
                "status": "pass",
                "duration_ms": 1,
                "failure_excerpt": null
            }],
            "summary": {"total": 1, "pass": 1, "fail": 0, "skipped": 0},
            "error": null
        });
        fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract(&format!("tests/binary.rs ({selector})"));

        let evidence = run_candidate_gate(fixture.request(
            &candidate,
            Some(&contract),
            Some(&tester),
            &artifact,
        ))
        .unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Passed);
        assert_eq!(
            check(&evidence, "tester-reference-evidence").status,
            CandidateCheckStatus::Passed
        );
    }

    #[test]
    fn matches_shell_script_without_description_literals() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/lifecycle.sh",
            "#!/bin/sh\nset -eu\nexit 0\n",
            "Add lifecycle script\n\n- Exercise lifecycle commands",
        );
        let contract = behavior_contract("tests/lifecycle.sh (help documents drain lifecycle)");
        let tester = fixture.artifacts.path().join("tester-results.json");
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let source_gate =
            run_candidate_gate(fixture.request(&candidate, Some(&contract), None, &artifact))
                .unwrap();
        assert_eq!(
            source_gate.disposition,
            CandidateGateDisposition::Passed,
            "{source_gate:#?}"
        );
        assert_eq!(
            check(&source_gate, "tester-reference-evidence").status,
            CandidateCheckStatus::Skipped
        );

        for harness in ["shell-harness", "project lifecycle checks"] {
            for (path, status, accepted) in [
                ("tests/lifecycle.sh", "pass", true),
                ("tests/other.sh", "pass", false),
                ("lifecycle.sh", "pass", false),
                ("tests/./lifecycle.sh", "pass", false),
                (
                    "tests/lifecycle.sh (help documents drain lifecycle)",
                    "pass",
                    false,
                ),
                ("help documents drain lifecycle", "pass", false),
                ("tests/lifecycle.sh", "fail", false),
                ("tests/lifecycle.sh", "skipped", false),
            ] {
                let results = serde_json::json!({
                    "candidate_commit": candidate,
                    "commands": [],
                    "tests": [{"id": path, "test_harness": harness, "status": status}],
                    "summary": {"total": 1, "pass": usize::from(status == "pass"), "fail": usize::from(status == "fail"), "skipped": usize::from(status == "skipped")},
                    "error": null
                });
                fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
                let mut request =
                    fixture.request(&candidate, Some(&contract), Some(&tester), &artifact);
                request.phase = CandidateGatePhase::FinalTester;
                let evidence = run_candidate_gate(request).unwrap();
                assert_eq!(
                    check(&evidence, "behavior-references").status,
                    CandidateCheckStatus::Passed
                );
                assert_eq!(
                    evidence.disposition,
                    if accepted {
                        CandidateGateDisposition::Passed
                    } else {
                        CandidateGateDisposition::Rejected
                    },
                    "{harness}, {path}, {status}: {evidence:#?}"
                );
                let preserved: serde_json::Value =
                    serde_json::from_slice(&fs::read(&tester).unwrap()).unwrap();
                assert_eq!(preserved, results);
            }
        }

        // A passing path-shaped result cannot replace a committed script.
        let missing = behavior_contract("tests/missing.sh (help documents drain lifecycle)");
        let mut results = serde_json::json!({
            "candidate_commit": candidate,
            "commands": [],
            "tests": [{"id": "tests/missing.sh", "test_harness": "shell-harness", "status": "pass"}],
            "summary": {"total": 1, "pass": 1, "fail": 0, "skipped": 0},
            "error": null
        });
        fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
        let evidence = run_candidate_gate(fixture.request(
            &candidate,
            Some(&missing),
            Some(&tester),
            &artifact,
        ))
        .unwrap();
        assert_eq!(
            check(&evidence, "behavior-references").status,
            CandidateCheckStatus::Failed
        );
        assert!(
            check(&evidence, "behavior-references")
                .diagnostics
                .iter()
                .any(|line| line.contains("missing committed test path tests/missing.sh"))
        );

        // The display label cannot make malformed structured evidence valid.
        results["tests"][0]["id"] = "tests/lifecycle.sh".into();
        results["tests"][0]
            .as_object_mut()
            .unwrap()
            .remove("status");
        fs::write(&tester, serde_json::to_vec(&results).unwrap()).unwrap();
        let evidence = run_candidate_gate(fixture.request(
            &candidate,
            Some(&contract),
            Some(&tester),
            &artifact,
        ))
        .unwrap();
        assert_eq!(evidence.disposition, CandidateGateDisposition::Blocked);
        assert_eq!(
            check(&evidence, "tester-reference-evidence").status,
            CandidateCheckStatus::Error
        );
        assert!(
            check(&evidence, "tester-reference-evidence")
                .diagnostics
                .iter()
                .any(|line| line.contains("cannot parse"))
        );
    }

    #[test]
    fn shell_evidence_uses_the_harness_native_script_identity() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/behaviors/operations/test-dashboard.sh",
            "#!/bin/sh\n# preserves selection\n",
            "Add dashboard journey\n\n- Cover dashboard selection",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        fs::write(
            &tester,
            format!(
                "{{\"candidate_commit\":\"{candidate}\",\"commands\":[],\"tests\":[{{\"id\":\"tests/behaviors/operations/test-dashboard.sh\",\"test_harness\":\"shell-harness\",\"status\":\"pass\",\"duration_ms\":null,\"failure_excerpt\":null}}],\"summary\":{{\"total\":1,\"pass\":1,\"fail\":0,\"skipped\":0}},\"error\":null}}"
            ),
        )
        .unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract =
            behavior_contract("tests/behaviors/operations/test-dashboard.sh (preserves selection)");

        let evidence = run_candidate_gate(fixture.request(
            &candidate,
            Some(&contract),
            Some(&tester),
            &artifact,
        ))
        .unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Passed);
    }

    #[test]
    fn malformed_structured_tester_evidence_blocks_instead_of_rejecting_candidate() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "tests/dashboard.rs",
            "#[test]\nfn preserves_selection() {}\n",
            "Add dashboard test",
        );
        let tester = fixture.artifacts.path().join("tester-results.json");
        fs::write(&tester, "not json\n").unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");
        let contract = behavior_contract("tests/dashboard.rs (preserves_selection)");

        let evidence = run_candidate_gate(fixture.request(
            &candidate,
            Some(&contract),
            Some(&tester),
            &artifact,
        ))
        .unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Blocked);
        assert_eq!(
            check(&evidence, "tester-reference-evidence").status,
            CandidateCheckStatus::Error
        );
    }

    #[test]
    fn project_check_failure_rejects_candidate_and_keeps_its_log() {
        let fixture = Fixture::new();
        let candidate = fixture.commit("src/lib.rs", "pub fn value() {}\n", "Add value");
        let hooks = fixture.project.path().join(".fluent/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("check-pre-merge");
        fs::write(
            &hook,
            "#!/bin/sh\nprintf 'dependency cycle: app -> render -> app\\n'\nexit 9\n",
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");

        let evidence =
            run_candidate_gate(fixture.request(&candidate, None, None, &artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Rejected);
        let project = check(&evidence, "project-check");
        assert_eq!(project.status, CandidateCheckStatus::Failed);
        assert!(project.log_path.as_ref().is_some_and(|path| {
            fs::read_to_string(Path::new(path))
                .unwrap()
                .contains("dependency cycle")
        }));
    }

    #[test]
    fn project_check_receives_the_effective_candidate_base() {
        let fixture = Fixture::new();
        let candidate = fixture.commit("src/lib.rs", "pub fn value() {}\n", "Add value");
        let hooks = fixture.project.path().join(".fluent/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("check-pre-merge");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\n[ \"$FLUENT_BASE_COMMIT\" = \"{}\" ] || exit 9\n",
                fixture.base
            ),
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");

        let evidence =
            run_candidate_gate(fixture.request(&candidate, None, None, &artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Passed);
        assert_eq!(
            check(&evidence, "project-check").status,
            CandidateCheckStatus::Passed
        );
    }

    #[test]
    fn a_project_check_that_mutates_the_candidate_blocks_as_invalid_configuration() {
        let fixture = Fixture::new();
        let candidate = fixture.commit("src/lib.rs", "pub fn value() {}\n", "Add value");
        let hooks = fixture.project.path().join(".fluent/hooks");
        fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("check-pre-merge");
        fs::write(&hook, "#!/bin/sh\nprintf dirty > hook-output.txt\nexit 0\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let artifact = fixture.artifacts.path().join("candidate-gate.json");

        let evidence =
            run_candidate_gate(fixture.request(&candidate, None, None, &artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Blocked);
        assert_eq!(
            check(&evidence, "project-check").status,
            CandidateCheckStatus::Error
        );
    }

    fn rejected_reference_gate(diagnostics: &[&str]) -> CandidateGateEvidence {
        CandidateGateEvidence {
            schema_version: CANDIDATE_GATE_SCHEMA_VERSION,
            phase: CandidateGatePhase::FinalTester,
            disposition: CandidateGateDisposition::Rejected,
            base_commit: "base".to_string(),
            candidate_commit: "candidate".to_string(),
            tester_results_path: Some("tester-results.json".to_string()),
            metadata_rewrite: None,
            checks: vec![CandidateCheckEvidence {
                name: "tester-reference-evidence".to_string(),
                status: CandidateCheckStatus::Failed,
                diagnostics: diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.to_string())
                    .collect(),
                log_path: None,
            }],
        }
    }

    #[test]
    fn rejected_gate_requests_one_exact_command_for_host_evidence() {
        let evidence = rejected_reference_gate(&[
            "no passing Tester result matches python3 tests/fluent_merge_readiness.py",
            "no passing Tester result matches python3 tests/fluent_merge_readiness.py",
            "no passing Tester result matches tests/readiness.py::current_frontier",
        ]);

        assert_eq!(
            requested_host_evidence_command(&evidence).as_deref(),
            Some("python3 tests/fluent_merge_readiness.py")
        );
    }

    #[test]
    fn rejected_gate_does_not_request_ambiguous_or_unrelated_host_evidence() {
        let multiple_commands = rejected_reference_gate(&[
            "no passing Tester result matches python3 tests/one.py",
            "no passing Tester result matches python3 tests/two.py",
        ]);
        assert_eq!(requested_host_evidence_command(&multiple_commands), None);

        let mut unrelated_failure =
            rejected_reference_gate(&["no passing Tester result matches python3 tests/one.py"]);
        unrelated_failure.checks.push(CandidateCheckEvidence {
            name: "candidate-identity".to_string(),
            status: CandidateCheckStatus::Failed,
            diagnostics: vec!["candidate is dirty".to_string()],
            log_path: None,
        });
        assert_eq!(requested_host_evidence_command(&unrelated_failure), None);

        let selector_only = rejected_reference_gate(&[
            "no passing Tester result matches tests/readiness.py::current_frontier",
        ]);
        assert_eq!(requested_host_evidence_command(&selector_only), None);
    }
}
