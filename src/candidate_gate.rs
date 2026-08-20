//! Run cheap, host-owned checks before Fluent spends a model-review cycle.
//! Recheck corrective candidates from their original base and admit a history
//! rewrite only when it repairs metadata without changing the candidate tree.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

use crate::work_model::{WriterCompletionContract, WriterCompletionRequirementKind};

pub const CANDIDATE_GATE_SCHEMA_VERSION: u32 = 4;
pub const CANDIDATE_GATE_FILE: &str = "candidate-gate.json";
const CANDIDATE_GATE_MAX_BYTES: u64 = 256 * 1024;

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CandidateMetadataRewriteAuthorization {
    #[default]
    RejectedMetadataGate,
    WriterCompletionRepair,
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
    pub tester_results_path: Option<&'a Path>,
    pub artifact_path: &'a Path,
}

pub fn run_candidate_gate(request: CandidateGateRequest<'_>) -> Result<CandidateGateEvidence> {
    run_candidate_gate_with_prior(request, None)
}

pub(crate) fn run_candidate_gate_with_prior(
    request: CandidateGateRequest<'_>,
    prior_candidate_gate_path: Option<&Path>,
) -> Result<CandidateGateEvidence> {
    let identity = check_candidate_identity(&request, prior_candidate_gate_path)?;
    let mut checks = vec![
        identity.check,
        check_whitespace(&request, &identity.effective_base_commit)?,
        check_commit_metadata(&request, &identity.effective_base_commit)?,
    ];
    checks.extend(check_behavior_references(&request)?);
    checks.push(run_project_check(&request)?);

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
        match prior_candidate_gate_path {
            Some(path) => match read_prior_writer_gate(path) {
                Ok(prior) => {
                    if let Err(reason) = validate_prior_writer_gate(&prior, request.base_commit) {
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
                        if !current_descends_from_rejected {
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
                if is_ancestor(
                    request.workspace,
                    request.base_commit,
                    request.candidate_commit,
                )? {
                    // The normal append-only Writer path needs no recovery.
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

fn validate_prior_writer_gate(
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

/// Return whether a prior host-owned gate explicitly permits replacing the
/// current candidate commit solely to repair its commit metadata.
///
/// Prompt guidance treats every missing, malformed, mismatched, or otherwise
/// untrusted gate as append-only. The candidate gate still performs the final
/// tree-identity verification after the Writer produces a replacement.
pub(crate) fn authorizes_tree_identical_metadata_rewrite(
    path: &Path,
    expected_candidate: &str,
) -> bool {
    let Ok(prior) = read_prior_writer_gate(path) else {
        return false;
    };
    if validate_prior_writer_gate(&prior, expected_candidate).is_err() {
        return false;
    }
    prior
        .checks
        .iter()
        .filter(|check| check.status == CandidateCheckStatus::Failed)
        .map(|check| check.name.as_str())
        .eq(["commit-metadata"])
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
    if failed != ["commit-metadata"] {
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
    Ok(Ok(CandidateMetadataRewriteEvidence {
        prior_gate_path: Some(evidence_path(request.project_root, prior_gate_path)),
        authorization: CandidateMetadataRewriteAuthorization::RejectedMetadataGate,
        original_base_commit: prior.base_commit.clone(),
        rejected_candidate_commit: prior.candidate_commit.clone(),
        replacement_candidate_commit: request.candidate_commit.to_string(),
        candidate_tree: replacement_tree,
    }))
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
    let range = format!("{}..{}", base_commit, request.candidate_commit);
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
        let mut message_lines = message.lines();
        let _subject = message_lines.next();
        let has_body = message_lines
            .next()
            .is_some_and(|separator| separator.trim().is_empty())
            && message_lines.any(|line| !line.trim().is_empty());
        if !has_body {
            diagnostics.push(format!(
                "commit {commit} has no body separated from its subject by a blank line"
            ));
        }
        if message.contains("\\n") {
            diagnostics.push(format!(
                "commit {commit} contains literal \\n text instead of line breaks"
            ));
        }
        for (index, line) in message.lines().enumerate().skip(1) {
            if line.chars().count() > 72 {
                diagnostics.push(format!(
                    "commit {commit} body line {} exceeds 72 characters",
                    index + 1
                ));
            }
        }
        if message.lines().any(|line| {
            line.trim_start()
                .to_ascii_lowercase()
                .starts_with("co-authored-by:")
        }) {
            diagnostics.push(format!(
                "commit {commit} contains a prohibited Co-Authored-By trailer"
            ));
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
                Ok((path, selector)) => {
                    let object = format!("{}:{path}", request.candidate_commit);
                    let output = crate::git::run_raw(request.workspace, &["show", &object])?;
                    if !output.status.success() {
                        diagnostics.push(format!(
                            "{} references missing committed test path {path}",
                            requirement.source
                        ));
                        continue;
                    }
                    if let Some(selector) = selector.as_deref()
                        && !String::from_utf8_lossy(&output.stdout).contains(selector)
                    {
                        diagnostics.push(format!(
                            "{} references test identifier {selector:?}, which is absent from {path}",
                            requirement.source
                        ));
                        continue;
                    }
                    test_references.push(TestReference { path, selector });
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
}

fn parse_test_reference(reference: &str) -> std::result::Result<(String, Option<String>), String> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err("reference is empty".to_string());
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
    let parsed = Path::new(path);
    if parsed.is_absolute()
        || path.is_empty()
        || parsed
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err("path must be a normalized project-relative path".to_string());
    }
    if selector
        .as_ref()
        .is_some_and(|selector| selector.is_empty())
    {
        return Err("test identifier is empty".to_string());
    }
    Ok((path.to_string(), selector))
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
    let mut missing_references = Vec::new();
    for reference in references {
        let matched = results.tests.iter().any(|test| {
            if test.status != "pass" {
                return false;
            }
            if test.test_harness == "shell-harness" && test.id == reference.path {
                return true;
            }
            match reference.selector.as_deref() {
                Some(selector) => {
                    test.id == selector
                        || test.id.ends_with(&format!("::{selector}"))
                        || test.id.ends_with(&format!("${selector}"))
                }
                None => test.id == reference.path,
            }
        });
        if !matched {
            missing_references.push(format!(
                "no passing Tester result matches {}",
                reference
                    .selector
                    .as_deref()
                    .unwrap_or(reference.path.as_str())
            ));
        }
    }
    let mut diagnostics = Vec::new();
    if !missing_references.is_empty() {
        let artifact_dir = path.parent().unwrap_or(path);
        for (index, command) in results
            .commands
            .iter()
            .enumerate()
            .filter(|(_, command)| command.exit_code != 0)
        {
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

fn run_project_check(request: &CandidateGateRequest<'_>) -> Result<CandidateCheckEvidence> {
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
                tester_results_path,
                artifact_path,
            }
        }
    }

    fn behavior_contract(reference: &str) -> WriterCompletionContract {
        WriterCompletionContract {
            version: WRITER_COMPLETION_CONTRACT_VERSION,
            requirements: vec![WriterCompletionRequirement {
                id: "behavior:area:b1".to_string(),
                kind: WriterCompletionRequirementKind::Behavior,
                source: "behaviors.md#area:b1".to_string(),
                requirement: "THE SYSTEM SHALL work.".to_string(),
                verification_refs: vec![reference.to_string()],
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
        assert_eq!(
            parse_test_reference("tests/example.rs::module::works").unwrap(),
            (
                "tests/example.rs".to_string(),
                Some("module::works".to_string())
            )
        );
        assert_eq!(
            parse_test_reference("tests/example.rs (module::works)").unwrap(),
            (
                "tests/example.rs".to_string(),
                Some("module::works".to_string())
            )
        );
        assert_eq!(
            parse_test_reference("tests/behaviors/example.sh").unwrap(),
            ("tests/behaviors/example.sh".to_string(), None)
        );
        assert!(parse_test_reference("tests/example.rs::").is_err());
    }

    #[test]
    fn rejects_whitespace_errors_and_prohibited_commit_metadata() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "src/lib.rs",
            "pub fn value() {    \n}\n",
            "Add value\\n\\nCo-Authored-By: Bot <bot@example.invalid>",
        );
        let artifact = fixture.artifacts.path().join("candidate-gate.json");

        let evidence =
            run_candidate_gate(fixture.request(&candidate, None, None, &artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Rejected);
        assert_eq!(
            check(&evidence, "whitespace").status,
            CandidateCheckStatus::Failed
        );
        assert_eq!(
            check(&evidence, "commit-metadata").status,
            CandidateCheckStatus::Failed
        );
        let parsed: CandidateGateEvidence =
            serde_json::from_slice(&fs::read(artifact).unwrap()).unwrap();
        assert_eq!(parsed, evidence);
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
        let prior =
            run_candidate_gate(fixture.request(&rejected, None, None, &prior_artifact)).unwrap();
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
        run_candidate_gate(fixture.request(&rejected, None, None, &prior_artifact)).unwrap();
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
    fn blocks_history_rewrite_after_mixed_gate_failures() {
        let fixture = Fixture::new();
        let rejected = fixture.commit(
            "src/lib.rs",
            "pub fn value() {    \n}\n",
            "Add value\\n\\nExplain value",
        );
        let prior_artifact = fixture.artifacts.path().join("prior-gate.json");
        run_candidate_gate(fixture.request(&rejected, None, None, &prior_artifact)).unwrap();
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
    fn rejects_overlong_commit_body_lines() {
        let fixture = Fixture::new();
        let candidate = fixture.commit(
            "src/lib.rs",
            "pub fn value() {}\n",
            &format!("Add value\n\n- {}", "x".repeat(73)),
        );
        let artifact = fixture.artifacts.path().join("candidate-gate.json");

        let evidence =
            run_candidate_gate(fixture.request(&candidate, None, None, &artifact)).unwrap();

        assert_eq!(evidence.disposition, CandidateGateDisposition::Rejected);
        assert!(
            check(&evidence, "commit-metadata")
                .diagnostics
                .iter()
                .any(|line| line.contains("exceeds 72 characters"))
        );
    }

    #[test]
    fn rejects_commit_without_body() {
        let subject_only_fixture = Fixture::new();
        let subject_only = subject_only_fixture.commit(
            "src/subject_only.rs",
            "pub fn subject_only() {}\n",
            "Add subject-only candidate",
        );
        let whitespace_only_fixture = Fixture::new();
        let whitespace_only = whitespace_only_fixture.commit(
            "src/whitespace_only.rs",
            "pub fn whitespace_only() {}\n",
            "Add whitespace-only candidate\n\n   ",
        );
        let valid_fixture = Fixture::new();
        let valid = valid_fixture.commit(
            "src/valid.rs",
            "pub fn valid() {}\n",
            "Add valid candidate\n\n- Explain the candidate",
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
            CandidateCheckStatus::Failed
        );

        let whitespace_only_artifact = whitespace_only_fixture
            .artifacts
            .path()
            .join("whitespace-only.json");
        let whitespace_only_evidence = run_candidate_gate(whitespace_only_fixture.request(
            &whitespace_only,
            None,
            None,
            &whitespace_only_artifact,
        ))
        .unwrap();
        assert_eq!(
            check(&whitespace_only_evidence, "commit-metadata").status,
            CandidateCheckStatus::Failed
        );

        let valid_artifact = valid_fixture.artifacts.path().join("valid.json");
        let valid_evidence =
            run_candidate_gate(valid_fixture.request(&valid, None, None, &valid_artifact)).unwrap();
        assert_eq!(
            check(&valid_evidence, "commit-metadata").status,
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
        let rejected = fixture.commit(
            "src/lib.rs",
            "pub fn value() {}\n",
            "Add value\\n\\nExplain value",
        );
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
                "no passing Tester result matches preserves_selection".to_string(),
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
}
