//! Assess current verification facts without scheduling or publishing state.
use crate::check_applicability::resolve_check_applicability;
use crate::work_model::{
    EvidenceDocument, EvidenceRecovery, EvidenceRecoveryState, EvidenceResult, Task, TaskKind,
    TaskStatus,
};
use crate::{review, work_model};
use anyhow::{Context, Result, bail};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Component, Path, PathBuf};
const MAX_EVIDENCE_BYTES: usize = 1024 * 1024;
pub(crate) fn candidate_workspace_path(
    project_root: &Path,
    output: &work_model::TaskOutput,
) -> PathBuf {
    let workspace = Path::new(&output.workspace_path);
    if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        project_root.join(workspace)
    }
}

pub(crate) fn validate_managed_file_path(project_root: &Path, relative: &str) -> Result<PathBuf> {
    let relative_path = Path::new(relative);
    let managed_root = Path::new(".fluent/work/artifacts");
    if relative_path.as_os_str().is_empty()
        || relative_path.is_absolute()
        || !relative_path.starts_with(managed_root)
    {
        bail!("artifact path must stay under the managed artifact root");
    }
    let components = relative_path.components().collect::<Vec<_>>();
    if components.len() <= managed_root.components().count()
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("artifact path must use normal managed components");
    }
    let full_path = project_root.join(relative_path);
    let mut current = project_root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            unreachable!("validated artifact component");
        };
        current.push(part);
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            bail!("artifact path must not traverse a symlink");
        }
        let is_last = index + 1 == components.len();
        if (is_last && !metadata.is_file()) || (!is_last && !metadata.is_dir()) {
            bail!("artifact path has an invalid file type");
        }
    }
    Ok(full_path)
}

pub(crate) fn parse_evidence_document(bytes: &[u8]) -> Result<EvidenceDocument> {
    if bytes.is_empty() || bytes.len() > MAX_EVIDENCE_BYTES {
        bail!("evidence document must be between 1 and {MAX_EVIDENCE_BYTES} bytes");
    }
    let document: EvidenceDocument = serde_json::from_slice(bytes)
        .context("evidence document must be valid schema-version-1 JSON")?;
    if document.schema_version != 1
        || document.producer.trim().is_empty()
        || document.check.trim().is_empty()
        || document.working_directory.trim().is_empty()
    {
        bail!("evidence document has an unsupported version or empty required field");
    }
    DateTime::parse_from_rfc3339(&document.run_at).context("evidence run_at must be RFC3339")?;
    Ok(document)
}

pub(crate) fn review_artifact_path(task: &Task) -> Option<String> {
    task.artifact_area
        .as_ref()
        .map(|area| format!("{}/review.md", area.path))
}
/// Evaluate canonical checks without publishing state or scheduling work.
pub(crate) struct CanonicalCheckAssessment {
    pub results_path: PathBuf,
    pub checks: Vec<work_model::RequiredCheck>,
    pub outstanding: Vec<usize>,
    pub requested: Vec<UnresolvedReviewCheck>,
    pub accepted: Vec<usize>,
    pub blockers: Vec<String>,
    pub diagnoses: Vec<String>,
    /// Demonstrated assertion or harness defects retain source correction.
    pub defects: Vec<String>,
    pub reconstruction: Option<CanonicalReconstruction>,
}

pub(crate) struct CanonicalReconstruction {
    pub manifest: work_model::BoundRecheckArtifact,
    pub manifest_bytes: Vec<u8>,
    pub audit: work_model::BoundRecheckArtifact,
    pub audit_bytes: Vec<u8>,
}

impl CanonicalCheckAssessment {
    pub fn is_satisfied(&self) -> bool {
        self.outstanding.is_empty() && self.blockers.is_empty() && self.defects.is_empty()
    }
}

pub(crate) fn bound_recovery_artifact(
    project_root: &Path,
    path: &str,
) -> Result<(work_model::BoundRecheckArtifact, Vec<u8>)> {
    let resolved = validate_managed_file_path(project_root, path)?;
    let bytes = fs::read(resolved)?;
    Ok((
        work_model::BoundRecheckArtifact {
            path: path.to_string(),
            digest: format!("sha256:{:x}", Sha256::digest(&bytes)),
        },
        bytes,
    ))
}

/// Select by candidate and configured command position; Task identities only
/// locate the immutable execution that supplies the complete-run evidence.
pub(crate) fn assess_canonical_checks(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<CanonicalCheckAssessment> {
    let output = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .context("no completed Writer candidate")?;
    let tester = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| {
            task.kind == TaskKind::Tester
                && task.status == TaskStatus::Complete
                && !task.is_failed_command_recheck()
                && task
                    .review_context
                    .as_ref()
                    .is_some_and(|context| context.candidate_commit == output.commit)
        })
        .context("no complete canonical Tester for the current candidate")?;
    let context = tester
        .review_context
        .as_ref()
        .context("Tester comparison context is missing")?;
    let area = &tester
        .artifact_area
        .as_ref()
        .context("Tester artifact area is missing")?
        .path;
    anyhow::ensure!(
        *area == work_model::work_artifact_path(&attempt.work_item_id, &attempt.id, &tester.id),
        "Tester artifact area is not canonical"
    );
    let (results_ref, result_bytes) =
        bound_recovery_artifact(project_root, &format!("{area}/tester-results.json"))?;
    let results: crate::tester::TesterResults = serde_json::from_slice(&result_bytes)?;
    anyhow::ensure!(
        results.candidate_commit.as_deref() == Some(output.commit.as_str()),
        "canonical Tester result has a different or missing candidate binding"
    );
    let workspace = candidate_workspace_path(project_root, output);
    let committed_bytes = |path: &str| -> Result<Vec<u8>> {
        let committed =
            crate::git::run_raw(&workspace, &["show", &format!("{}:{path}", output.commit)])?;
        anyhow::ensure!(
            committed.status.success(),
            "cannot read committed checker configuration: {path}"
        );
        Ok(committed.stdout)
    };
    let config_bytes = committed_bytes(".fluent/tester.yaml")?;
    let extractor_bytes = committed_bytes(".fluent/extract-tester-results")?;
    let original_manifest_path = format!("{area}/canonical-run.json");
    let mut reconstruction = None;
    let (manifest_ref, manifest_bytes) = match fs::symlink_metadata(
        project_root.join(&original_manifest_path),
    ) {
        Ok(_) => bound_recovery_artifact(project_root, &original_manifest_path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Support one older shape: an exact candidate-bound completed full
            // Tester whose complete ordered commands prove default code checks.
            let manifest = reconstruct_legacy_manifest(&config_bytes, &extractor_bytes, &results)?;
            let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
            let directory = work_model::work_artifact_path(
                &attempt.work_item_id,
                &attempt.id,
                &format!("evidence-reconstructions/{}", tester.id),
            );
            let manifest_ref = work_model::BoundRecheckArtifact {
                path: format!("{directory}/canonical-run.json"),
                digest: format!("sha256:{:x}", Sha256::digest(&manifest_bytes)),
            };
            let audit_bytes = serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": 1, "work_item_id": attempt.work_item_id, "attempt_id": attempt.id,
                "candidate_commit": output.commit, "source_task_id": tester.id,
                "source_task_kind": tester.kind, "source_task_role": tester.role,
                "source_task_context": context, "source_results": results_ref,
                "configuration": {"git_object":format!("{}:.fluent/tester.yaml", output.commit), "digest":manifest.config_digest},
                "extractor": {"git_object":format!("{}:.fluent/extract-tester-results", output.commit), "digest":manifest.extractor_digest},
                "derived_manifest": manifest_ref,
                "basis": "Completed canonical Tester and exact ordered command coverage; default candidate-code applicability only"
            }))?;
            anyhow::ensure!(
                audit_bytes.len() <= MAX_EVIDENCE_BYTES,
                "legacy reconstruction audit exceeds its bound"
            );
            let audit = work_model::BoundRecheckArtifact {
                path: format!("{directory}/audit.json"),
                digest: format!("sha256:{:x}", Sha256::digest(&audit_bytes)),
            };
            for (artifact, expected) in [(&manifest_ref, &manifest_bytes), (&audit, &audit_bytes)] {
                match fs::symlink_metadata(project_root.join(&artifact.path)) {
                    Ok(_) => anyhow::ensure!(
                        bound_recovery_artifact(project_root, &artifact.path)?.1 == *expected,
                        "preserved legacy reconstruction bytes changed: {}",
                        artifact.path
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            reconstruction = Some(CanonicalReconstruction {
                manifest: manifest_ref.clone(),
                manifest_bytes: manifest_bytes.clone(),
                audit,
                audit_bytes,
            });
            (manifest_ref, manifest_bytes)
        }
        Err(error) => return Err(error.into()),
    };
    let manifest: crate::tester::CanonicalRunManifest = serde_json::from_slice(&manifest_bytes)?;
    anyhow::ensure!(
        manifest.schema_version == 1 && manifest.full_run,
        "Tester manifest does not prove a complete canonical run"
    );
    for (path, bytes, expected) in [
        (
            ".fluent/tester.yaml",
            &config_bytes,
            &manifest.config_digest,
        ),
        (
            ".fluent/extract-tester-results",
            &extractor_bytes,
            &manifest.extractor_digest,
        ),
    ] {
        anyhow::ensure!(
            &format!("sha256:{:x}", Sha256::digest(bytes)) == expected,
            "canonical check configuration changed: {path}"
        );
        if path == ".fluent/tester.yaml" {
            let config: crate::tester::TesterConfig = serde_yaml::from_slice(bytes)?;
            anyhow::ensure!(
                config.commands == manifest.commands,
                "canonical manifest differs from configured commands"
            );
        }
    }
    let mut blockers = Vec::new();
    if results.commands.len() != manifest.commands.len() {
        blockers.push("canonical Tester is missing configured command results".into());
    }
    let mut defects = Vec::new();
    if let Some(error) = &results.error {
        defects.push(format!(
            "canonical Tester has an unresolved extractor or harness error: {}",
            error.kind
        ));
    }
    if results.summary.total != results.tests.len()
        || results.summary.pass
            != results
                .tests
                .iter()
                .filter(|test| test.status == "pass")
                .count()
        || results.summary.fail
            != results
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
        || results.summary.skipped
            != results
                .tests
                .iter()
                .filter(|test| matches!(test.status.as_str(), "skipped" | "not_run"))
                .count()
    {
        blockers
            .push("canonical Tester summary does not account for its structured results".into());
    }
    let mut logs = Vec::new();
    for (index, command) in results.commands.iter().enumerate() {
        for (actual, expected) in [
            (&command.stdout_log, format!("commands/{index}-stdout.log")),
            (&command.stderr_log, format!("commands/{index}-stderr.log")),
        ] {
            anyhow::ensure!(
                actual == &expected,
                "canonical command log path does not match command {index}"
            );
            logs.push(bound_recovery_artifact(project_root, &format!("{area}/{expected}"))?.0);
        }
        if command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
        {
            blockers.push(format!("command {index} has unresolved process leaks"));
        }
    }
    let requested = unresolved_review_checks(project_root, attempt);
    blockers.extend(requested.iter().map(|check| check.reason.clone()));
    let reviewed = accepted_review_check_results(project_root, attempt);
    let mut checks = Vec::new();
    let mut outstanding = Vec::new();
    let mut accepted = Vec::new();
    let mut diagnoses = Vec::new();
    for (index, definition) in manifest.commands.iter().enumerate() {
        let applicability = match manifest.applicability.get(index) {
            Some(binding) => binding.clone(),
            None if definition.applicability.is_default() => {
                work_model::ResolvedCheckApplicability::default()
            }
            None => bail!("required command applicability snapshot is missing"),
        };
        anyhow::ensure!(
            applicability.definition == definition.applicability
                && resolve_check_applicability(&workspace, &definition.applicability)?
                    == applicability,
            "declared comparison ref or check applicability changed since execution"
        );
        let check = work_model::RequiredCheck {
            applicability,
            id: format!(
                "tester:{}:{index}",
                manifest
                    .config_digest
                    .strip_prefix("sha256:")
                    .context("invalid Tester configuration digest")?
            ),
            work_item_id: attempt.work_item_id.clone(),
            attempt_id: attempt.id.clone(),
            candidate_commit: output.commit.clone(),
            command: definition.command.clone(),
            working_directory: ".".into(),
            source: work_model::RequiredCheckSource::CanonicalTester {
                reconstruction: reconstruction.as_ref().map(|proof| proof.audit.clone()),
                task_id: tester.id.clone(),
                command_index: index,
                results: results_ref.clone(),
                manifest: manifest_ref.clone(),
                logs: logs.clone(),
                comparison_commit: context.base_commit.clone(),
            },
        };
        let result = results.commands.get(index);
        let matches = result.is_some_and(|command| {
            command.command == definition.command && command.test_harness == definition.test_harness
        });
        if !matches {
            blockers.push(format!(
                "command {index} does not match the canonical manifest"
            ));
        }
        let unmet_prerequisite = result.and_then(|command| match &command.failure_cause {
            Some(crate::tester::CommandFailureCause::UnmetPrerequisite { fact, required, observed })
                if matches && command.exit_code != 0 && command.duration_ms == 0
                    && matches!(fact.as_str(), "os" | "architecture")
                    && definition.applicability.environment.get(fact) == Some(required)
                    && manifest.environment.get(index).and_then(|facts| facts.get(fact)) == Some(observed)
                    && observed != required => Some(format!("{fact}: required {required}, observed {observed}; prerequisite prevented execution")),
            _ => None,
        });
        if definition.reject_process_leaks
            && unmet_prerequisite.is_none()
            && result.is_none_or(|command| command.process_settlement.is_none())
        {
            blockers.push(format!(
                "command {index} has no required process-settlement proof"
            ));
        }
        let local_evidence = EvidenceDocument {
            schema_version: 1,
            producer: tester.id.clone(),
            check: check.command.clone(),
            working_directory: check.working_directory.clone(),
            result: EvidenceResult::Pass,
            output: String::new(),
            run_at: manifest.run_at.clone().unwrap_or_default(),
            environment: manifest.environment.get(index).cloned().unwrap_or_default(),
            comparison_commits: check.applicability.comparison_commits.clone(),
        };
        let local_applies = validate_check_applicability_at(
            &check.applicability,
            &local_evidence,
            crate::check_applicability::evaluation_time(),
        )
        .is_ok();
        if !matches || !local_applies || result.is_none_or(|command| command.exit_code != 0) {
            let reviewed_replacement = manifest
                .commands
                .iter()
                .filter(|other| other.command == definition.command)
                .count()
                == 1
                && reviewed.iter().any(|result| {
                    result.command == check.command
                        && result.applicability == check.applicability
                        && result.comparison_commit == context.base_commit
                });
            if matches
                && (required_check_is_accepted(project_root, attempt, &check)
                    || reviewed_replacement)
            {
                accepted.push(index);
            } else {
                outstanding.push(index);
                if let Some(reason) = unmet_prerequisite {
                    diagnoses.push(format!("Required check {}: {reason}. Repair the declared environment or submit authorized host proof.", check.id));
                } else if results.error.is_none()
                    && !results.tests.iter().any(|test| test.status == "fail")
                    && result.is_some_and(|command| command.exit_code != 0)
                {
                    diagnoses.push(format!("Required check {} needs diagnosis: a nonzero exit does not establish an environment, source, or harness cause. Preserve the logs and establish the missing cause or submit exact authorized host proof.", check.id));
                }
            }
        }
        checks.push(check);
    }
    for test in results.tests.iter().filter(|test| test.status == "fail") {
        // Only the established command identity can be discharged by a reviewed
        // replacement. A harness match alone cannot attribute an assertion.
        let covered = accepted.iter().any(|index| {
            let command = &manifest.commands[*index];
            test.id == format!("command-{index}:{}", command.command)
                && test.test_harness == command.test_harness
        });
        if !covered {
            defects.push(format!(
                "canonical Tester has an unresolved structured failure: {}",
                test.id
            ));
        }
    }
    Ok(CanonicalCheckAssessment {
        requested,
        defects,
        reconstruction,
        diagnoses,
        results_path: project_root.join(results_ref.path),
        checks,
        outstanding,
        accepted,
        blockers,
    })
}

pub(crate) fn required_check_is_accepted(
    project_root: &Path,
    attempt: &work_model::Attempt,
    check: &work_model::RequiredCheck,
) -> bool {
    validate_required_check_acceptance_at(
        project_root,
        attempt,
        check,
        crate::check_applicability::evaluation_time(),
    )
    .is_ok()
}

pub(crate) fn validate_required_check_acceptance_at(
    project_root: &Path,
    attempt: &work_model::Attempt,
    check: &work_model::RequiredCheck,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    // A newer conflicting result must receive its own review; the older pass
    // cannot silently defeat the operator's subsequently submitted evidence.
    let recovery = attempt
        .evidence_recoveries
        .iter()
        .rev()
        .find(|recovery| {
            recovery.required_check.as_ref().is_some_and(|bound| {
                bound.id == check.id && bound.candidate_commit == check.candidate_commit
            })
        })
        .context("required check has no recorded evidence")?;
    anyhow::ensure!(
        recovery.required_check.as_ref() == Some(check)
            && recovery.state == EvidenceRecoveryState::Complete
            && recovery.targets.len() == 1,
        "required check needs completed targeted evidence"
    );
    let (attachment, bytes) =
        bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path)?;
    anyhow::ensure!(
        attachment.digest == recovery.attachment.digest,
        "host evidence bytes changed"
    );
    let document = parse_evidence_document(&bytes)?;
    validate_check_applicability_at(&check.applicability, &document, now)?;
    anyhow::ensure!(
        document.result == EvidenceResult::Pass
            && document.check == check.command
            && document.working_directory == check.working_directory,
        "host evidence does not pass this exact check"
    );
    let target = &recovery.targets[0];
    let task = attempt
        .tasks
        .iter()
        .find(|task| Some(task.id.as_str()) == target.review_task_id.as_deref())
        .context("missing targeted review")?;
    let context = task
        .evidence_review_context
        .as_ref()
        .context("missing evidence review context")?;
    anyhow::ensure!(
        task.kind == TaskKind::Review
            && task.role == "tests"
            && task.status == TaskStatus::Complete
            && context.recovery_id == recovery.id
            && context.candidate_commit == check.candidate_commit
            && context.attachment == recovery.attachment
            && context.prior_review_artifact == target.prior_review_artifact,
        "targeted review binding changed"
    );
    let (_, binding) = bound_recovery_artifact(project_root, &target.prior_review_artifact)?;
    match &check.source {
        work_model::RequiredCheckSource::CanonicalTester {
            results,
            manifest,
            logs,
            reconstruction,
            ..
        } => {
            anyhow::ensure!(
                serde_json::from_slice::<work_model::RequiredCheck>(&binding)? == *check,
                "required check snapshot changed"
            );
            for source in std::iter::once(results)
                .chain(std::iter::once(manifest))
                .chain(logs)
                .chain(reconstruction)
            {
                anyhow::ensure!(
                    bound_recovery_artifact(project_root, &source.path)?
                        .0
                        .digest
                        == source.digest,
                    "required source proof is missing or changed: {}",
                    source.path
                );
            }
        }
        work_model::RequiredCheckSource::ReviewRequested { request, .. } => anyhow::ensure!(
            request.path == target.prior_review_artifact
                && request.digest == format!("sha256:{:x}", Sha256::digest(&binding)),
            "review request snapshot changed"
        ),
    }
    let path = format!(
        "{}/review.md",
        task.artifact_area
            .as_ref()
            .context("missing review artifacts")?
            .path
    );
    let (artifact, bytes) = bound_recovery_artifact(project_root, &path)?;
    anyhow::ensure!(
        target.review_digest.as_deref() == Some(artifact.digest.as_str())
            && review::extract_verdict(std::str::from_utf8(&bytes)?) == review::Verdict::Pass,
        "targeted review has no accepted passing result"
    );
    Ok(())
}

pub(crate) fn validate_check_applicability_at(
    binding: &work_model::ResolvedCheckApplicability,
    document: &EvidenceDocument,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    for (name, expected) in &binding.definition.environment {
        anyhow::ensure!(
            document.environment.get(name) == Some(expected),
            "required environment fact {name} is missing or changed"
        );
    }
    anyhow::ensure!(
        binding
            .comparison_commits
            .iter()
            .all(|(name, commit)| document.comparison_commits.get(name) == Some(commit)),
        "required comparison commit is missing or changed"
    );
    if let work_model::CheckFreshness::LiveState { max_age_seconds } = binding.definition.freshness
    {
        let seconds = max_age_seconds
            .filter(|seconds| *seconds > 0 && *seconds <= i64::MAX as u64)
            .context("live-state evidence is missing a supported freshness expiry")?;
        let recorded = DateTime::parse_from_rfc3339(&document.run_at)?.with_timezone(&chrono::Utc);
        let expiry = chrono::TimeDelta::try_seconds(seconds as i64)
            .and_then(|duration| recorded.checked_add_signed(duration))
            .context("unsupported live-state freshness expiry")?;
        anyhow::ensure!(
            recorded <= now && now < expiry,
            "live-state evidence has expired or has an unverified future timestamp"
        );
    }
    Ok(())
}

pub(crate) fn validate_check_applicability(
    workspace: &Path,
    binding: &work_model::ResolvedCheckApplicability,
    document: &EvidenceDocument,
) -> Result<()> {
    validate_check_applicability_in_workspace_at(
        workspace,
        binding,
        document,
        crate::check_applicability::evaluation_time(),
    )
}

pub(crate) fn validate_check_applicability_in_workspace_at(
    workspace: &Path,
    binding: &work_model::ResolvedCheckApplicability,
    document: &EvidenceDocument,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<()> {
    anyhow::ensure!(
        resolve_check_applicability(workspace, &binding.definition)? == *binding,
        "declared comparison ref changed since execution"
    );
    validate_check_applicability_at(binding, document, now)
}

pub(crate) fn validate_required_check_binding(
    project_root: &Path,
    attempt: &work_model::Attempt,
    check: &work_model::RequiredCheck,
) -> Result<()> {
    let output = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .context("required check has no current candidate")?;
    anyhow::ensure!(
        check.work_item_id == attempt.work_item_id
            && check.attempt_id == attempt.id
            && check.candidate_commit == output.commit,
        "required check belongs to another candidate or Attempt"
    );
    match &check.source {
        work_model::RequiredCheckSource::CanonicalTester {
            results,
            manifest,
            logs,
            reconstruction,
            ..
        } => {
            for source in std::iter::once(results)
                .chain(std::iter::once(manifest))
                .chain(logs)
                .chain(reconstruction)
            {
                anyhow::ensure!(
                    bound_recovery_artifact(project_root, &source.path)?.0 == *source,
                    "required source digest changed: {}",
                    source.path
                );
            }
            anyhow::ensure!(
                assess_canonical_checks(project_root, attempt)?
                    .checks
                    .contains(check),
                "required canonical check changed"
            );
        }
        work_model::RequiredCheckSource::ReviewRequested {
            task_id,
            request,
            comparison_commit,
        } => {
            let task = attempt
                .tasks
                .iter()
                .find(|task| {
                    task.id == *task_id
                        && task.kind == TaskKind::Review
                        && task.status == TaskStatus::Complete
                })
                .context("required reviewer request has no completed source Task")?;
            let context = task
                .review_context
                .as_ref()
                .context("reviewer request has no comparison context")?;
            anyhow::ensure!(
                context.candidate_commit == check.candidate_commit
                    && &context.base_commit == comparison_commit
                    && task
                        .artifact_area
                        .as_ref()
                        .is_some_and(|area| request.path == format!("{}/review.md", area.path)),
                "review request source binding changed"
            );
            let (actual, bytes) = bound_recovery_artifact(project_root, &request.path)?;
            anyhow::ensure!(actual == *request, "review request source bytes changed");
            let definition = review::extract_focused_evidence_request(std::str::from_utf8(&bytes)?)
                .context("review request is missing its command declaration")?;
            anyhow::ensure!(
                definition.command == check.command
                    && definition.working_directory == check.working_directory
                    && resolve_check_applicability(
                        &candidate_workspace_path(project_root, output),
                        &definition.applicability
                    )? == check.applicability,
                "review request command, environment, freshness, or comparison changed"
            );
        }
    }
    Ok(())
}

/// Reconstruct only inputs proved by the completed producer and committed code.
pub(crate) fn reconstruct_legacy_manifest(
    config_bytes: &[u8],
    extractor_bytes: &[u8],
    results: &crate::tester::TesterResults,
) -> Result<crate::tester::CanonicalRunManifest> {
    anyhow::ensure!(
        config_bytes.len() <= MAX_EVIDENCE_BYTES && extractor_bytes.len() <= 4 * MAX_EVIDENCE_BYTES,
        "legacy checker source exceeds the supported bound"
    );
    let config: crate::tester::TesterConfig = serde_yaml::from_slice(config_bytes)?;
    anyhow::ensure!(
        !config.commands.is_empty()
            && config.commands.len() <= 1024
            && config.commands.len() == results.commands.len(),
        "legacy Tester lacks complete ordered command coverage"
    );
    for (definition, result) in config.commands.iter().zip(&results.commands) {
        anyhow::ensure!(
            definition.applicability.is_default(),
            "legacy Tester lacks historical applicability proof; current environment facts or comparison refs cannot establish an older run's inputs"
        );
        anyhow::ensure!(
            definition.command == result.command && definition.test_harness == result.test_harness,
            "legacy Tester command or harness differs from its committed configuration"
        );
    }
    Ok(crate::tester::CanonicalRunManifest {
        schema_version: 1,
        config_digest: format!("sha256:{:x}", Sha256::digest(config_bytes)),
        extractor_digest: format!("sha256:{:x}", Sha256::digest(extractor_bytes)),
        commands: config.commands,
        full_run: true,
        run_at: None,
        environment: vec![],
        applicability: vec![],
    })
}

pub(crate) struct UnresolvedReviewCheck {
    pub check: work_model::RequiredCheck,
    pub reason: String,
    pub can_refresh: bool,
}

fn unresolved_review_checks(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Vec<UnresolvedReviewCheck> {
    let candidate = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .map(|output| output.commit.as_str());
    let mut seen = std::collections::HashSet::new();
    let mut unresolved = Vec::new();
    for check in attempt
        .evidence_recoveries
        .iter()
        .rev()
        .filter_map(|recovery| recovery.required_check.as_ref())
    {
        let work_model::RequiredCheckSource::ReviewRequested { request, .. } = &check.source else {
            continue;
        };
        if Some(check.candidate_commit.as_str()) != candidate || !seen.insert(check.id.clone()) {
            continue;
        }
        let accepted =
            validate_required_check_binding(project_root, attempt, check).and_then(|()| {
                validate_required_check_acceptance_at(
                    project_root,
                    attempt,
                    check,
                    crate::check_applicability::evaluation_time(),
                )
            });
        if let Err(error) = accepted {
            let can_refresh =
                expired_completed_review_recovery(project_root, attempt, &request.path)
                    .is_ok_and(|recovery| recovery.is_some());
            unresolved.push(UnresolvedReviewCheck {
                check: check.clone(),
                reason: format!("Required reviewer check {} remains unresolved: `{}` (working directory {}, source {}): {error:#}", check.id, check.command, check.working_directory, request.path),
                can_refresh,
            });
        }
    }
    unresolved
}

/// Authorize renewing an expired completed request only while every other
/// recorded binding, including its passing Review, remains intact.
pub(crate) fn expired_completed_review_recovery<'a>(
    project_root: &Path,
    attempt: &'a work_model::Attempt,
    review_artifact: &str,
) -> Result<Option<&'a EvidenceRecovery>> {
    let Some(recovery) = attempt.evidence_recoveries.iter().rev().find(|recovery| {
        recovery.required_check.as_ref().is_some_and(|check| matches!(&check.source,
            work_model::RequiredCheckSource::ReviewRequested { request, .. } if request.path == review_artifact))
    }) else { return Ok(None); };
    if recovery.state != EvidenceRecoveryState::Complete {
        return Ok(None);
    }
    let check = recovery.required_check.as_ref().unwrap();
    let latest_review = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Review && task.role == "tests")
        .context("required reviewer check has no latest Tests Review")?;
    let path =
        review_artifact_path(latest_review).context("latest Tests Review has no artifact")?;
    let (_, review_bytes) = bound_recovery_artifact(project_root, &path)?;
    anyhow::ensure!(
        latest_review.status == TaskStatus::Complete
            && latest_review
                .review_context
                .as_ref()
                .is_some_and(|context| context.candidate_commit == check.candidate_commit)
            && review::extract_verdict(std::str::from_utf8(&review_bytes)?)
                == review::Verdict::Pass,
        "the latest Tests Review must pass before renewing completed evidence"
    );

    validate_required_check_binding(project_root, attempt, check)?;
    let work_model::CheckFreshness::LiveState {
        max_age_seconds: Some(seconds),
    } = check.applicability.definition.freshness
    else {
        return Ok(None);
    };
    let (_, bytes) = bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path)?;
    let document = parse_evidence_document(&bytes)?;
    let issued = DateTime::parse_from_rfc3339(&document.run_at)?.with_timezone(&chrono::Utc);
    let expiry = chrono::TimeDelta::try_seconds(i64::try_from(seconds)?)
        .and_then(|duration| issued.checked_add_signed(duration))
        .context("unsupported live-state freshness expiry")?;
    if crate::check_applicability::evaluation_time() < expiry {
        return Ok(None);
    }
    validate_required_check_acceptance_at(project_root, attempt, check, issued)?;
    Ok(Some(recovery))
}

struct ReviewedCheckResult {
    command: String,
    applicability: work_model::ResolvedCheckApplicability,
    comparison_commit: Option<String>,
}

pub(crate) fn completed_review_check_commands(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Vec<String> {
    let mut commands = accepted_review_check_results(project_root, attempt)
        .into_iter()
        .map(|result| result.command)
        .collect::<Vec<_>>();
    commands.sort();
    commands.dedup();
    commands
}

fn accepted_review_check_results(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Vec<ReviewedCheckResult> {
    let latest_candidate = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .map(|output| output.commit.as_str());
    let Some(latest_candidate) = latest_candidate else {
        return Vec::new();
    };
    let read_verdict = |root: &Path, task: &Task| {
        let path = review_artifact_path(task)?;
        let (_, bytes) = bound_recovery_artifact(root, &path).ok()?;
        Some(review::extract_verdict(std::str::from_utf8(&bytes).ok()?))
    };
    let commands = attempt
        .evidence_recoveries
        .iter()
        .filter_map(|recovery| {
            if recovery.state != EvidenceRecoveryState::Complete
                || recovery.candidate_commit != latest_candidate
            {
                return None;
            }
            if let Some(check) = &recovery.required_check {
                if !matches!(
                    check.source,
                    work_model::RequiredCheckSource::ReviewRequested { .. }
                ) || validate_required_check_binding(project_root, attempt, check).is_err()
                    || !required_check_is_accepted(project_root, attempt, check)
                {
                    return None;
                }
            }
            let (_, evidence_bytes) =
                bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path).ok()?;
            if format!("sha256:{:x}", Sha256::digest(&evidence_bytes)) != recovery.attachment.digest
            {
                return None;
            }
            let evidence = parse_evidence_document(&evidence_bytes).ok()?;
            if evidence.result != EvidenceResult::Pass {
                return None;
            }

            recovery.targets.iter().find_map(|target| {
                if target.role != "tests" {
                    return None;
                }
                let (_, prior_bytes) =
                    bound_recovery_artifact(project_root, &target.prior_review_artifact).ok()?;
                let prior_review = std::str::from_utf8(&prior_bytes).ok()?;
                let request = review::extract_focused_evidence_request(&prior_review)?;
                if recovery.required_check.is_none() && !request.applicability.is_default() {
                    return None;
                }
                if evidence.check != request.command
                    || request.working_directory != "."
                    || evidence.working_directory != request.working_directory
                {
                    return None;
                }
                let prior_task_is_bound = attempt.tasks.iter().any(|task| {
                    task.kind == TaskKind::Review
                        && task.role == "tests"
                        && task.status == TaskStatus::Complete
                        && task
                            .review_context
                            .as_ref()
                            .map(|context| context.candidate_commit.as_str())
                            == Some(latest_candidate)
                        && review_artifact_path(task).as_deref()
                            == Some(target.prior_review_artifact.as_str())
                        && read_verdict(project_root, task) == Some(review::Verdict::Fail)
                });
                let targeted_review_passed = target.review_task_id.as_ref().is_some_and(|id| {
                    attempt.tasks.iter().any(|task| {
                        task.id == *id
                            && task.kind == TaskKind::Review
                            && task.role == "tests"
                            && task.status == TaskStatus::Complete
                            && task
                                .evidence_review_context
                                .as_ref()
                                .is_some_and(|context| {
                                    context.recovery_id == recovery.id
                                        && context.candidate_commit == recovery.candidate_commit
                                        && context.attachment == recovery.attachment
                                        && context.prior_review_artifact
                                            == target.prior_review_artifact
                                })
                            && read_verdict(project_root, task) == Some(review::Verdict::Pass)
                    })
                });
                let source_task = attempt.tasks.iter().find(|task| {
                    review_artifact_path(task).as_deref()
                        == Some(target.prior_review_artifact.as_str())
                })?;
                (prior_task_is_bound && targeted_review_passed).then_some(ReviewedCheckResult {
                    command: request.command,
                    applicability: recovery
                        .required_check
                        .as_ref()
                        .map(|check| check.applicability.clone())
                        .unwrap_or_default(),
                    comparison_commit: source_task.review_context.as_ref()?.base_commit.clone(),
                })
            })
        })
        .collect::<Vec<_>>();
    commands
}

use crate::verification_reference::{self as references, ReferenceMeaning, TestReference};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerificationCause {
    CandidateDefect,
    MissingEvidence,
    UninterpretableEvidence,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementAssessment {
    pub id: String,
    pub source: String,
    pub meaning: Option<ReferenceMeaning>,
    pub satisfied: bool,
    pub cause: Option<VerificationCause>,
    pub owner: work_model::CompletionResponsibility,
    pub next_action: Option<String>,
    pub bindings: Vec<work_model::BoundRecheckArtifact>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationAssessment {
    pub candidate_commit: String,
    pub references: Vec<RequirementAssessment>,
    pub commands: Vec<RequirementAssessment>,
}

impl VerificationAssessment {
    pub fn candidate_defects(&self) -> Vec<&RequirementAssessment> {
        self.references
            .iter()
            .chain(&self.commands)
            .filter(|row| !row.satisfied && row.cause == Some(VerificationCause::CandidateDefect))
            .collect()
    }

    pub fn unresolved(&self) -> impl Iterator<Item = &RequirementAssessment> {
        self.references
            .iter()
            .chain(&self.commands)
            .filter(|row| !row.satisfied)
    }
}

/// Evaluate one reference occurrence from immutable planning text. Result prose
/// explains the row; only the approved row, occurrence and source digest name it.
pub(crate) fn assess_references(
    workspace: &Path,
    work: &str,
    attempt: &str,
    candidate: &str,
    contract: &work_model::WriterCompletionContract,
    results: Option<&crate::tester::TesterResults>,
    canonical: Option<&CanonicalCheckAssessment>,
    bindings: &[work_model::BoundRecheckArtifact],
) -> Result<Vec<RequirementAssessment>> {
    let mut rows = Vec::new();
    for requirement in contract
        .requirements
        .iter()
        .filter(|row| row.kind == work_model::WriterCompletionRequirementKind::Behavior)
    {
        for (index, text) in requirement.verification_refs.iter().enumerate() {
            if text
                .strip_prefix("Untestable:")
                .is_some_and(|reason| !reason.trim().is_empty())
            {
                continue;
            }
            let mut row = RequirementAssessment {
                id: format!("reference:{work}:{attempt}:{}:{index}:{:x}", requirement.id, Sha256::digest(text.as_bytes())),
                source: requirement.source.clone(), meaning: None, satisfied: false,
                cause: Some(VerificationCause::UninterpretableEvidence),
                owner: work_model::CompletionResponsibility::Tester,
                next_action: Some("Inspect the approved reference and retained native evidence to establish its meaning; clarification recording is not available in this version.".into()),
                bindings: bindings.to_vec(), diagnostics: vec![],
            };
            let reference = match references::parse_test_reference(text) {
                Ok(reference) => reference,
                Err(reason) => {
                    row.diagnostics.push(format!(
                        "{} has malformed Test: reference {text:?}: {reason}",
                        requirement.source
                    ));
                    rows.push(row);
                    continue;
                }
            };
            row.meaning = Some(reference.meaning());
            if let Err(reason) = validate_reference_source(workspace, candidate, &reference) {
                row.cause = Some(VerificationCause::CandidateDefect);
                row.owner = requirement
                    .responsibility
                    .unwrap_or(work_model::CompletionResponsibility::Writer);
                row.next_action = Some(
                    "Repair the committed test or its approved verification reference.".into(),
                );
                row.diagnostics.push(reason.to_string());
                rows.push(row);
                continue;
            }
            let result =
                results.filter(|results| results.candidate_commit.as_deref() == Some(candidate));
            row.satisfied = result
                .is_some_and(|results| reference_is_satisfied(&reference, results, canonical));
            if row.satisfied {
                row.cause = None;
                row.next_action = None;
            } else {
                row.cause = Some(VerificationCause::MissingEvidence);
                row.next_action = Some("Inspect the exact native identities and command logs, then collect candidate-bound evidence through the existing verification recovery path.".into());
                if let Some(results) = result {
                    for (index, command) in
                        results
                            .commands
                            .iter()
                            .enumerate()
                            .filter(|(index, command)| {
                                command.exit_code != 0
                                    && canonical.is_none_or(|view| !view.accepted.contains(index))
                            })
                    {
                        row.diagnostics.push(format!(
                            "Tester command {index} exited {}: {}",
                            command.exit_code, command.command
                        ));
                        if let Some(directory) = bindings
                            .first()
                            .and_then(|binding| Path::new(&binding.path).parent())
                        {
                            row.diagnostics.push(format!(
                                "Tester command {index} stdout log: {}",
                                directory.join(&command.stdout_log).display()
                            ));
                            row.diagnostics.push(format!(
                                "Tester command {index} stderr log: {}",
                                directory.join(&command.stderr_log).display()
                            ));
                        }
                    }
                }
                row.diagnostics
                    .push(format!("no passing Tester result matches {text}"));
                if let Some(results) = result {
                    if reference.command.is_none() {
                        row.diagnostics.push(format!(
                            "observed Tester identities for {text} (showing {} of {}): {}",
                            results.tests.len().min(20),
                            results.tests.len(),
                            results
                                .tests
                                .iter()
                                .take(20)
                                .map(|test| format!("{:?} ({})", test.id, test.status))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                } else {
                    row.diagnostics
                        .push("Tester evidence is missing or bound to another candidate".into());
                }
            }
            rows.push(row);
        }
    }
    Ok(rows)
}

pub(crate) fn validate_reference_source(
    workspace: &Path,
    candidate: &str,
    reference: &TestReference,
) -> Result<()> {
    if reference.path.is_empty() && reference.command.is_some() {
        return Ok(());
    }
    let output = crate::git::run_raw(
        workspace,
        &["show", &format!("{candidate}:{}", reference.path)],
    )?;
    anyhow::ensure!(
        output.status.success(),
        "references missing committed test path {}",
        reference.path
    );
    anyhow::ensure!(
        reference.source_contains_selector(&String::from_utf8_lossy(&output.stdout)),
        "references test identifier {:?}, which is absent from {}",
        reference.selector,
        reference.path
    );
    Ok(())
}

fn reference_is_satisfied(
    reference: &TestReference,
    results: &crate::tester::TesterResults,
    canonical: Option<&CanonicalCheckAssessment>,
) -> bool {
    match reference.meaning() {
        ReferenceMeaning::Command {
            command,
            command_index,
            ..
        } => {
            let matches = results
                .commands
                .iter()
                .enumerate()
                .filter(|(index, result)| {
                    result.command == command
                        && command_index.is_none_or(|expected| expected == *index)
                })
                .collect::<Vec<_>>();
            let [(index, result)] = matches.as_slice() else {
                return false;
            };
            canonical.map_or(result.exit_code == 0, |view| {
                view.checks
                    .get(*index)
                    .is_some_and(|check| check.command == command)
                    && !view.outstanding.contains(index)
                    && view.blockers.is_empty()
            })
        }
        ReferenceMeaning::WholeScript { path, .. } => {
            let matching = results
                .tests
                .iter()
                .filter(|test| test.id == path)
                .collect::<Vec<_>>();
            !matching.is_empty() && matching.iter().all(|test| test.status == "pass")
        }
        ReferenceMeaning::NamedTest { path, name, .. } if path.ends_with(".sh") => {
            let matching = results
                .tests
                .iter()
                .filter(|test| test.id == format!("{path}::{name}"))
                .collect::<Vec<_>>();
            !matching.is_empty() && matching.iter().all(|test| test.status == "pass")
        }
        ReferenceMeaning::NamedTest { path, .. } if path.ends_with(".rs") => {
            let tests = results
                .tests
                .iter()
                .filter_map(|test| {
                    references::RustTestIdentity::parse(&test.id).map(|identity| (identity, test))
                })
                .collect::<Vec<_>>();
            reference.rust_identity().is_some_and(|expected| {
                references::rust_evidence_matches_reference(&expected, &tests)
            })
        }
        ReferenceMeaning::NamedTest { path, .. } if path.ends_with(".py") => {
            let tests = results
                .tests
                .iter()
                .filter_map(|test| {
                    references::PythonTestIdentity::parse(&test.id).map(|identity| (identity, test))
                })
                .collect::<Vec<_>>();
            reference.python_identity().is_some_and(|expected| {
                references::python_evidence_matches_reference(&expected, &tests)
            })
        }
        _ => false,
    }
}

/// Observe a saved Attempt without journal recovery, migration, or publication.
pub fn assess_current_verification(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Result<VerificationAssessment> {
    let canonical = assess_canonical_checks(project_root, attempt)?;
    let output = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .context("no candidate")?;
    let bytes = fs::read(&canonical.results_path)?;
    let results = serde_json::from_slice(&bytes)?;
    let bindings = vec![work_model::BoundRecheckArtifact {
        path: canonical
            .results_path
            .strip_prefix(project_root)?
            .to_string_lossy()
            .into_owned(),
        digest: format!("sha256:{:x}", Sha256::digest(&bytes)),
    }];
    let references = match &attempt.writer_completion_contract {
        Some(contract) => assess_references(
            &candidate_workspace_path(project_root, output),
            &attempt.work_item_id,
            &attempt.id,
            &output.commit,
            contract,
            Some(&results),
            Some(&canonical),
            &bindings,
        )?,
        None => vec![],
    };
    let mut commands = Vec::new();
    for (index, check) in canonical.checks.iter().enumerate() {
        let satisfied = !canonical.outstanding.contains(&index);
        commands.push(RequirementAssessment {
            id: check.id.clone(), source: "canonical Tester".into(),
            meaning: Some(ReferenceMeaning::Command { command: check.command.clone(), command_index: Some(index), description: None }),
            satisfied, cause: (!satisfied).then_some(VerificationCause::Unknown),
            owner: if canonical.accepted.contains(&index) { work_model::CompletionResponsibility::Host } else { work_model::CompletionResponsibility::Tester },
            next_action: (!satisfied).then(|| "Inspect the retained command logs and establish the cause before authorizing candidate correction.".into()),
            bindings: bindings.clone(), diagnostics: canonical.diagnoses.clone(),
        });
    }
    for (index, defect) in canonical.defects.iter().enumerate() {
        commands.push(RequirementAssessment {
            id: format!("tester-defect:{}:{index}", bindings[0].digest),
            source: bindings[0].path.clone(),
            meaning: None,
            satisfied: false,
            cause: Some(VerificationCause::CandidateDefect),
            owner: work_model::CompletionResponsibility::Writer,
            next_action: Some(
                "Repair the established assertion or project-owned harness defect.".into(),
            ),
            bindings: bindings.clone(),
            diagnostics: vec![defect.clone()],
        });
    }
    for (index, blocker) in canonical.blockers.iter().enumerate() {
        commands.push(RequirementAssessment {
            id: format!("tester-binding:{}:{index}", bindings[0].digest),
            source: bindings[0].path.clone(),
            meaning: None,
            satisfied: false,
            cause: Some(VerificationCause::UninterpretableEvidence),
            owner: work_model::CompletionResponsibility::Tester,
            next_action: Some(
                "Inspect and restore the named immutable proof before retrying verification."
                    .into(),
            ),
            bindings: bindings.clone(),
            diagnostics: vec![blocker.clone()],
        });
    }
    Ok(VerificationAssessment {
        candidate_commit: output.commit.clone(),
        references,
        commands,
    })
}
