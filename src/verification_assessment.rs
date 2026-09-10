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
/// Credit only errors produced by the candidate's configuration or extractor.
pub(crate) fn tester_error_is_candidate_defect(error: &crate::tester::TesterError) -> bool {
    matches!(
        error.kind.as_str(),
        "tester_yaml_problem"
            | "extractor_missing"
            | "extractor_failure"
            | "duplicate_test_ids"
            | "invalid_test_identity"
    )
}

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
    if relative_path.components().collect::<PathBuf>().as_os_str() != relative_path.as_os_str()
        || relative_path.as_os_str().is_empty()
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
    pub config_digest: String,
    pub results_path: PathBuf,
    pub checks: Vec<work_model::RequiredCheck>,
    pub outstanding: Vec<usize>,
    pub requested: Vec<UnresolvedReviewCheck>,
    pub accepted: Vec<usize>,
    pub review_bindings: std::collections::HashMap<usize, Vec<work_model::BoundRecheckArtifact>>,
    pub blockers: Vec<String>,
    pub diagnoses: Vec<String>,
    /// Demonstrated assertion or harness defects retain source correction.
    pub defects: Vec<CanonicalDefect>,
    pub reconstruction: Option<CanonicalReconstruction>,
}

pub(crate) struct CanonicalDefect {
    pub id: String,
    pub diagnostic: String,
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
    anyhow::ensure!(
        fs::metadata(&resolved)?.len() <= 16 * MAX_EVIDENCE_BYTES as u64,
        "verification artifact exceeds the bounded read limit: {path}"
    );
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
    assess_canonical_checks_at(
        project_root,
        attempt,
        crate::check_applicability::evaluation_time(),
    )
}

fn assess_canonical_checks_at(
    project_root: &Path,
    attempt: &work_model::Attempt,
    now: chrono::DateTime<chrono::Utc>,
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
        if tester_error_is_candidate_defect(error) {
            defects.push(CanonicalDefect {
                id: format!("tester-harness:{}:{}", manifest.config_digest, error.kind),
                diagnostic: format!(
                    "canonical Tester has an unresolved extractor or harness error: {}",
                    error.kind
                ),
            });
        } else {
            blockers.push(format!(
                "canonical Tester needs infrastructure diagnosis: {}",
                error.kind
            ));
        }
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
    let requested = unresolved_review_checks(project_root, attempt, now);
    blockers.extend(requested.iter().map(|check| check.reason.clone()));
    let reviewed = accepted_review_check_results(project_root, attempt, now);
    let rechecked = attempt
        .evidence_recoveries
        .iter()
        .filter_map(|recovery| accepted_recheck_result(project_root, attempt, recovery))
        .collect::<Vec<_>>();
    let mut checks = Vec::new();
    let mut outstanding = Vec::new();
    let mut accepted = Vec::new();
    let mut review_bindings = std::collections::HashMap::new();
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
        let local_applies =
            validate_check_applicability_at(&check.applicability, &local_evidence, now).is_ok();
        if !matches || !local_applies || result.is_none_or(|command| command.exit_code != 0) {
            let unique_command = manifest
                .commands
                .iter()
                .filter(|other| other.command == definition.command)
                .count()
                == 1;
            let reviewed_replacement = reviewed.iter().find(|result| {
                unique_command
                    && result.command == check.command
                    && result.applicability == check.applicability
                    && result.comparison_commit == context.base_commit
            });
            let recheck_replacement = rechecked.iter().find(|proof| {
                proof.source_results == results_ref
                    && proof.command_index == index
                    && proof.command == check.command
                    && proof.comparison_commit == context.base_commit
                    && check.applicability == work_model::ResolvedCheckApplicability::default()
            });
            if matches
                && (validate_required_check_acceptance_at(project_root, attempt, &check, now)
                    .is_ok()
                    || reviewed_replacement.is_some()
                    || recheck_replacement.is_some())
            {
                accepted.push(index);
                if let Some(proof) = reviewed_replacement {
                    review_bindings.insert(index, proof.bindings.clone());
                } else if let Some(proof) = recheck_replacement {
                    review_bindings.insert(index, proof.bindings.clone());
                }
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
            let native = references::normalized_result(test)
                .map(|test| test.id)
                .unwrap_or_else(|_| test.id.clone());
            defects.push(CanonicalDefect {
                id: format!(
                    "tester-failure:{}:{}",
                    manifest.config_digest,
                    crate::verification_policy::digest(native.as_bytes())
                ),
                diagnostic: format!(
                    "canonical Tester has an unresolved structured failure: {}",
                    test.id
                ),
            });
        }
    }
    defects.sort_by(|a, b| a.id.cmp(&b.id));
    defects.dedup_by(|a, b| a.id == b.id);
    Ok(CanonicalCheckAssessment {
        config_digest: manifest.config_digest,
        requested,
        defects,
        reconstruction,
        diagnoses,
        results_path: project_root.join(results_ref.path),
        checks,
        outstanding,
        accepted,
        review_bindings,
        blockers,
    })
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
    now: chrono::DateTime<chrono::Utc>,
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
                validate_required_check_acceptance_at(project_root, attempt, check, now)
            });
        if let Err(error) = accepted {
            let can_refresh =
                expired_completed_review_recovery_at(project_root, attempt, &request.path, now)
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
    expired_completed_review_recovery_at(
        project_root,
        attempt,
        review_artifact,
        crate::check_applicability::evaluation_time(),
    )
}

fn expired_completed_review_recovery_at<'a>(
    project_root: &Path,
    attempt: &'a work_model::Attempt,
    review_artifact: &str,
    now: chrono::DateTime<chrono::Utc>,
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
    if now < expiry {
        return Ok(None);
    }
    validate_required_check_acceptance_at(project_root, attempt, check, issued)?;
    Ok(Some(recovery))
}

struct ReviewedCheckResult {
    command: String,
    applicability: work_model::ResolvedCheckApplicability,
    comparison_commit: Option<String>,
    bindings: Vec<work_model::BoundRecheckArtifact>,
}

pub(crate) fn completed_review_check_commands(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Vec<String> {
    let mut commands = accepted_review_check_results(
        project_root,
        attempt,
        crate::check_applicability::evaluation_time(),
    )
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
    now: chrono::DateTime<chrono::Utc>,
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
                    || validate_required_check_acceptance_at(project_root, attempt, check, now)
                        .is_err()
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
                    bindings: vec![
                        bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path)
                            .ok()?
                            .0,
                        bound_recovery_artifact(project_root, &target.prior_review_artifact)
                            .ok()?
                            .0,
                        bound_recovery_artifact(
                            project_root,
                            &review_artifact_path(
                                attempt.tasks.iter().find(|task| {
                                    Some(&task.id) == target.review_task_id.as_ref()
                                })?,
                            )?,
                        )
                        .ok()?
                        .0,
                    ],
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
    pub check: Option<work_model::RequiredCheck>,
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
    accepted: &[AcceptedCommandProof],
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
                source: requirement.source.clone(), check: None, meaning: None, satisfied: false,
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
            let declared_command_is_bound = if let Some(ReferenceMeaning::Command {
                command,
                command_index,
                ..
            }) = &reference.declared
            {
                let config = crate::git::run_raw(
                    workspace,
                    &["show", &format!("{candidate}:.fluent/tester.yaml")],
                )?;
                let config: Option<crate::tester::TesterConfig> = config
                    .status
                    .success()
                    .then(|| serde_yaml::from_slice(&config.stdout).ok())
                    .flatten();
                config.is_some_and(|config| {
                    let entries = config
                        .commands
                        .iter()
                        .enumerate()
                        .filter(|(index, entry)| {
                            entry.command == *command
                                && command_index.is_none_or(|expected| expected == *index)
                        })
                        .collect::<Vec<_>>();
                    let [(index, entry)] = entries.as_slice() else {
                        return false;
                    };
                    result.is_some_and(|results| {
                        results.commands.get(*index).is_some_and(|result| {
                            result.command == entry.command
                                && result.test_harness == entry.test_harness
                        })
                    })
                })
            } else {
                true
            };
            row.satisfied = declared_command_is_bound
                && result
                    .is_some_and(|results| reference_is_satisfied(&reference, results, canonical));
            // A configured legacy command shares the canonical occurrence's
            // ambiguity, applicability, and comparison checks. The historical
            // adapter applies only after a current run proves the command lies
            // outside its configured checks.
            if reference.declared.is_none()
                && canonical.is_some_and(|view| {
                    !view
                        .checks
                        .iter()
                        .any(|check| Some(check.command.as_str()) == reference.command.as_deref())
                })
            {
                if let Some(proof) = accepted
                    .iter()
                    .find(|proof| reference.command.as_deref() == Some(proof.command.as_str()))
                {
                    row.satisfied = true;
                    row.owner = work_model::CompletionResponsibility::Host;
                    row.bindings.extend(proof.bindings.clone());
                }
            }
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
        reference.selector.is_none()
            || reference.source_contains_selector(&String::from_utf8_lossy(&output.stdout)),
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
    let normalized = results
        .tests
        .iter()
        .map(references::normalized_result)
        .collect::<std::result::Result<Vec<_>, _>>();
    let Ok(normalized) = normalized else {
        return false;
    };
    let mut results = results.clone();
    results.tests = normalized;
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
            let [(index, _result)] = matches.as_slice() else {
                return false;
            };
            canonical.is_some_and(|view| {
                view.checks
                    .get(*index)
                    .is_some_and(|check| check.command == command)
                    && !view.outstanding.contains(index)
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
    assess_current_verification_at(
        project_root,
        attempt,
        crate::check_applicability::evaluation_time(),
    )
}

pub fn assess_current_verification_at(
    project_root: &Path,
    attempt: &work_model::Attempt,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<VerificationAssessment> {
    let output = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .context("no candidate")?;
    // Absence of a run for a new candidate is pending work. Historical runs
    // neither satisfy its requirements nor prevent its ordinary verification.
    if !attempt.tasks.iter().any(|task| {
        task.kind == TaskKind::Tester
            && task.status == TaskStatus::Complete
            && !task.is_failed_command_recheck()
            && task
                .review_context
                .as_ref()
                .is_some_and(|context| context.candidate_commit == output.commit)
    }) {
        return Ok(VerificationAssessment {
            candidate_commit: output.commit.clone(),
            references: match &attempt.writer_completion_contract {
                Some(contract) => assess_references(
                    &candidate_workspace_path(project_root, output),
                    &attempt.work_item_id,
                    &attempt.id,
                    &output.commit,
                    contract,
                    None,
                    None,
                    &[],
                    &[],
                )?,
                None => vec![],
            },
            commands: vec![],
        });
    }
    let canonical = assess_canonical_checks_at(project_root, attempt, now)?;
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
    let mut references = match &attempt.writer_completion_contract {
        Some(contract) => assess_references(
            &candidate_workspace_path(project_root, output),
            &attempt.work_item_id,
            &attempt.id,
            &output.commit,
            contract,
            Some(&results),
            Some(&canonical),
            &bindings,
            &accepted_legacy_commands(project_root, attempt, now),
        )?,
        None => vec![],
    };
    let mut commands = Vec::new();
    for (index, check) in canonical.checks.iter().enumerate() {
        let mut supporting = bindings.clone();
        supporting.extend(
            canonical
                .review_bindings
                .get(&index)
                .into_iter()
                .flatten()
                .cloned(),
        );
        if let work_model::RequiredCheckSource::CanonicalTester {
            manifest,
            logs,
            reconstruction,
            ..
        } = &check.source
        {
            supporting.push(manifest.clone());
            supporting.extend(logs.clone());
            supporting.extend(reconstruction.clone());
        }
        if canonical.accepted.contains(&index) {
            if let Some(recovery) = attempt
                .evidence_recoveries
                .iter()
                .rev()
                .find(|recovery| recovery.required_check.as_ref() == Some(check))
            {
                supporting.push(work_model::BoundRecheckArtifact {
                    path: recovery.attachment.snapshot_path.clone(),
                    digest: recovery.attachment.digest.clone(),
                });
                for target in &recovery.targets {
                    supporting.push(
                        bound_recovery_artifact(project_root, &target.prior_review_artifact)?.0,
                    );
                    if let Some(task) = attempt
                        .tasks
                        .iter()
                        .find(|task| target.review_task_id.as_deref() == Some(task.id.as_str()))
                    {
                        if let Some(path) = review_artifact_path(task) {
                            supporting.push(bound_recovery_artifact(project_root, &path)?.0);
                        }
                    }
                }
            }
        }
        let satisfied = !canonical.outstanding.contains(&index);
        commands.push(RequirementAssessment {
            id: check.id.clone(), source: "canonical Tester".into(), check: Some(check.clone()),
            meaning: Some(ReferenceMeaning::Command { command: check.command.clone(), command_index: Some(index), description: None }),
            satisfied, cause: (!satisfied).then_some(VerificationCause::Unknown),
            owner: if canonical.accepted.contains(&index) { work_model::CompletionResponsibility::Host } else { work_model::CompletionResponsibility::Tester },
            next_action: (!satisfied).then(|| "Inspect the retained command logs and establish the cause before authorizing candidate correction.".into()),
            bindings: supporting, diagnostics: canonical.diagnoses.clone(),
        });
    }
    for defect in &canonical.defects {
        commands.push(RequirementAssessment {
            id: defect.id.clone(),
            source: bindings[0].path.clone(),
            meaning: None,
            check: None,
            satisfied: false,
            cause: Some(VerificationCause::CandidateDefect),
            owner: work_model::CompletionResponsibility::Writer,
            next_action: Some(
                "Repair the established assertion or project-owned harness defect.".into(),
            ),
            bindings: bindings.clone(),
            diagnostics: vec![defect.diagnostic.clone()],
        });
    }
    if !canonical.blockers.is_empty() {
        commands.push(RequirementAssessment {
            id: format!(
                "tester-binding:{}:{}:{}",
                attempt.work_item_id, attempt.id, canonical.config_digest
            ),
            source: bindings[0].path.clone(),
            meaning: None,
            check: None,
            satisfied: false,
            cause: Some(VerificationCause::UninterpretableEvidence),
            owner: work_model::CompletionResponsibility::Tester,
            next_action: Some(
                "Inspect and restore the named immutable proof before retrying verification."
                    .into(),
            ),
            bindings: bindings.clone(),
            diagnostics: canonical.blockers.clone(),
        });
    }
    for reference in &mut references {
        if let Some(ReferenceMeaning::Command {
            command,
            command_index,
            ..
        }) = &reference.meaning
        {
            let matches = commands.iter().filter(|row| matches!(&row.meaning, Some(ReferenceMeaning::Command { command: actual, command_index: actual_index, .. }) if actual == command && command_index.is_none_or(|index| Some(index) == *actual_index))).collect::<Vec<_>>();
            if let [row] = matches.as_slice() {
                reference.check = row.check.clone();
                for binding in &row.bindings {
                    if !reference.bindings.contains(binding) {
                        reference.bindings.push(binding.clone());
                    }
                }
                if reference.satisfied {
                    reference.owner = row.owner;
                }
            }
        }
    }
    Ok(VerificationAssessment {
        candidate_commit: output.commit.clone(),
        references,
        commands,
    })
}

pub(crate) fn snapshot_path(work: &str, attempt: &str, digest: &str) -> Result<String> {
    let hex = digest
        .strip_prefix("sha256:")
        .context("invalid evidence digest")?;
    Ok(format!(
        ".fluent/work/artifacts/{work}/{attempt}/host-evidence/{hex}.json"
    ))
}

pub const VERIFICATION_GATE_SCHEMA_VERSION: u32 = 5;

/// Adapt the one supported legacy gate shape without depending on gate execution.
pub(crate) fn requested_legacy_host_command(gate: &serde_json::Value) -> Option<String> {
    let version = gate.get("schema_version")?.as_u64()?;
    if version == 0
        || version > u64::from(VERIFICATION_GATE_SCHEMA_VERSION)
        || gate.get("phase")?.as_str()? != "final-tester"
        || gate.get("disposition")?.as_str()? != "rejected"
    {
        return None;
    }
    let blocking = gate
        .get("checks")?
        .as_array()?
        .iter()
        .filter(|check| {
            matches!(
                check.get("status").and_then(|status| status.as_str()),
                Some("failed" | "error")
            )
        })
        .collect::<Vec<_>>();
    let [check] = blocking.as_slice() else {
        return None;
    };
    if check.get("name")?.as_str()? != "tester-reference-evidence"
        || check.get("status")?.as_str()? != "failed"
    {
        return None;
    }
    let mut commands = check
        .get("diagnostics")?
        .as_array()?
        .iter()
        .filter_map(|diagnostic| {
            let reference = references::parse_test_reference(
                diagnostic
                    .as_str()?
                    .strip_prefix("no passing Tester result matches ")?,
            )
            .ok()?;
            // Explicit commands name canonical occurrences; this adapter only
            // preserves legacy commands checked outside the configured Tester.
            if reference.declared.is_some() {
                return None;
            }
            reference.command
        })
        .collect::<Vec<_>>();
    commands.sort();
    commands.dedup();
    let [command] = commands.as_slice() else {
        return None;
    };
    Some(command.clone())
}

pub(crate) struct AcceptedCommandProof {
    pub command: String,
    bindings: Vec<work_model::BoundRecheckArtifact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyGateBinding {
    schema_version: u32,
    candidate_commit: String,
    gate_artifact: work_model::BoundRecheckArtifact,
    gate_evidence: serde_json::Value,
    command: String,
    working_directory: String,
}

pub(crate) fn accepted_legacy_gate_commands(
    project_root: &Path,
    attempt: &work_model::Attempt,
) -> Vec<AcceptedCommandProof> {
    let candidate = attempt
        .tasks
        .iter()
        .rev()
        .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
        .and_then(|task| task.output.as_ref())
        .map(|output| output.commit.as_str());
    attempt
        .evidence_recoveries
        .iter()
        .filter_map(|recovery| {
            if recovery.state != EvidenceRecoveryState::Complete
                || Some(recovery.candidate_commit.as_str()) != candidate
                || recovery.required_check.is_some()
            {
                return None;
            }
            let [target] = recovery.targets.as_slice() else {
                return None;
            };
            if target.role != "tests" {
                return None;
            }
            let (source, bytes) =
                bound_recovery_artifact(project_root, &target.prior_review_artifact).ok()?;
            let binding: LegacyGateBinding = serde_json::from_slice(&bytes).ok()?;
            if binding.schema_version != 1
                || binding.candidate_commit != recovery.candidate_commit
                || snapshot_path(&attempt.work_item_id, &attempt.id, &source.digest).ok()?
                    != source.path
                || binding.gate_evidence.get("candidate_commit")?.as_str()?
                    != recovery.candidate_commit
                || requested_legacy_host_command(&binding.gate_evidence).as_deref()
                    != Some(binding.command.as_str())
                || binding.working_directory != "."
            {
                return None;
            }
            let (attachment, bytes) =
                bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path).ok()?;
            if attachment.digest != recovery.attachment.digest {
                return None;
            }
            let evidence = parse_evidence_document(&bytes).ok()?;
            if evidence.result != EvidenceResult::Pass
                || evidence.check != binding.command
                || evidence.working_directory != binding.working_directory
            {
                return None;
            }
            let review = attempt.tasks.iter().find(|task| {
                Some(&task.id) == target.review_task_id.as_ref()
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
                                && context.prior_review_artifact == target.prior_review_artifact
                        })
            })?;
            let (review_source, bytes) =
                bound_recovery_artifact(project_root, &review_artifact_path(review)?).ok()?;
            if review::extract_verdict(std::str::from_utf8(&bytes).ok()?) != review::Verdict::Pass {
                return None;
            }
            Some(AcceptedCommandProof {
                command: binding.command,
                bindings: vec![source, binding.gate_artifact, attachment, review_source],
            })
        })
        .collect()
}

pub(crate) fn accepted_legacy_commands(
    project_root: &Path,
    attempt: &work_model::Attempt,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<AcceptedCommandProof> {
    let mut proofs = accepted_legacy_gate_commands(project_root, attempt);
    proofs.extend(
        accepted_review_check_results(project_root, attempt, now)
            .into_iter()
            .map(|result| AcceptedCommandProof {
                command: result.command,
                bindings: result.bindings,
            }),
    );
    proofs
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FailedCommandRecheckEvidenceBinding {
    pub(crate) schema_version: u32,
    pub(crate) candidate_commit: String,
    pub(crate) recheck_task_id: String,
    pub(crate) source_results: work_model::BoundRecheckArtifact,
    pub(crate) recheck_artifacts: Vec<work_model::BoundRecheckArtifact>,
    pub(crate) command_index: usize,
    pub(crate) command: String,
    /// The command the authorized host actually ran. Older bindings used the
    /// recheck command for both fields, so absence retains that interpretation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) evidence_check: Option<String>,
    pub(crate) working_directory: String,
}

struct AcceptedRecheckResult {
    source_results: work_model::BoundRecheckArtifact,
    command_index: usize,
    command: String,
    comparison_commit: Option<String>,
    bindings: Vec<work_model::BoundRecheckArtifact>,
}

fn accepted_recheck_result(
    project_root: &Path,
    attempt: &work_model::Attempt,
    recovery: &EvidenceRecovery,
) -> Option<AcceptedRecheckResult> {
    if recovery.state != EvidenceRecoveryState::Complete
        || recovery.targets.len() != 1
        || recovery.targets[0].role != "tests"
    {
        return None;
    }
    let target = &recovery.targets[0];
    let binding_bytes = bound_recovery_artifact(project_root, &target.prior_review_artifact)
        .ok()?
        .1;
    let binding: FailedCommandRecheckEvidenceBinding =
        serde_json::from_slice(&binding_bytes).ok()?;
    let binding_digest = format!("sha256:{:x}", Sha256::digest(&binding_bytes));
    if binding.schema_version != 1
        || binding.candidate_commit != recovery.candidate_commit
        || snapshot_path(
            attempt.work_item_id.as_str(),
            attempt.id.as_str(),
            &binding_digest,
        )
        .ok()
        .as_deref()
            != Some(target.prior_review_artifact.as_str())
        || attempt
            .tasks
            .iter()
            .rev()
            .find(|task| task.kind == TaskKind::Write && task.status == TaskStatus::Complete)
            .and_then(|task| task.output.as_ref())
            .map(|output| output.commit.as_str())
            != Some(recovery.candidate_commit.as_str())
    {
        return None;
    }
    let evidence_bytes = bound_recovery_artifact(project_root, &recovery.attachment.snapshot_path)
        .ok()?
        .1;
    if format!("sha256:{:x}", Sha256::digest(&evidence_bytes)) != recovery.attachment.digest {
        return None;
    }
    let evidence = parse_evidence_document(&evidence_bytes).ok()?;
    let evidence_check = binding
        .evidence_check
        .as_deref()
        .unwrap_or(binding.command.as_str());
    if evidence.result != EvidenceResult::Pass
        || evidence.check != evidence_check
        || evidence_check != binding.command
        || evidence.working_directory != binding.working_directory
    {
        return None;
    }
    let review_task = target.review_task_id.as_ref().and_then(|id| {
        attempt.tasks.iter().find(|task| {
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
                            && context.prior_review_artifact == target.prior_review_artifact
                            && context.attachment == recovery.attachment
                    })
        })
    })?;
    let review_path = review_artifact_path(review_task)?;
    let (review_binding, review_bytes) =
        bound_recovery_artifact(project_root, &review_path).ok()?;
    if review::extract_verdict(std::str::from_utf8(&review_bytes).ok()?) != review::Verdict::Pass
        || target
            .review_digest
            .as_ref()
            .is_some_and(|digest| digest != &review_binding.digest)
    {
        return None;
    }
    let recheck = attempt.tasks.iter().find(|task| {
        task.id == binding.recheck_task_id
            && task.kind == TaskKind::Tester
            && task.status == TaskStatus::Complete
            && task.is_failed_command_recheck()
    })?;
    let current_artifacts =
        failed_command_recheck_correction_artifacts(project_root, recheck).ok()?;
    if current_artifacts.len() != binding.recheck_artifacts.len()
        || !current_artifacts
            .iter()
            .zip(&binding.recheck_artifacts)
            .all(|(current, bound)| {
                current.path == bound.path
                    && bound_recovery_artifact(project_root, &current.path).is_ok_and(
                        |(_, bytes)| format!("sha256:{:x}", Sha256::digest(&bytes)) == bound.digest,
                    )
            })
    {
        return None;
    }
    let source_bytes = bound_recovery_artifact(project_root, &binding.source_results.path)
        .ok()?
        .1;
    if format!("sha256:{:x}", Sha256::digest(&source_bytes)) != binding.source_results.digest {
        return None;
    }
    let source: crate::tester::TesterResults = serde_json::from_slice(&source_bytes).ok()?;
    let command = source.commands.get(binding.command_index)?;
    let area = &recheck.artifact_area.as_ref()?.path;
    let (_, manifest_bytes) =
        bound_recovery_artifact(project_root, &format!("{area}/failed-command-recheck.json"))
            .ok()?;
    let manifest: crate::tester::FailedCommandRecheckManifest =
        serde_json::from_slice(&manifest_bytes).ok()?;
    let (_, recheck_bytes) =
        bound_recovery_artifact(project_root, &format!("{area}/tester-results.json")).ok()?;
    let recheck_results: crate::tester::TesterResults =
        serde_json::from_slice(&recheck_bytes).ok()?;
    let position = manifest
        .failed_command_indexes
        .iter()
        .position(|index| *index == binding.command_index)?;
    let checked = recheck_results.commands.get(position)?;
    if manifest.source_results != binding.source_results.path
        || manifest.candidate_commit != binding.candidate_commit
        || binding.working_directory != "."
        || checked.command != command.command
        || checked.test_harness != command.test_harness
    {
        return None;
    }

    if source.candidate_commit.as_deref() != Some(recovery.candidate_commit.as_str())
        || source.error.is_some()
        || command.exit_code == 0
        || command
            .process_settlement
            .as_ref()
            .is_some_and(|settlement| !settlement.leaks.is_empty())
        || command.command != binding.command
        || source.summary.fail
            != source
                .tests
                .iter()
                .filter(|test| test.status == "fail")
                .count()
    {
        return None;
    }
    let mut bindings = binding.recheck_artifacts.clone();
    bindings.extend([
        binding.source_results.clone(),
        review_binding,
        work_model::BoundRecheckArtifact {
            path: target.prior_review_artifact.clone(),
            digest: binding_digest,
        },
        work_model::BoundRecheckArtifact {
            path: recovery.attachment.snapshot_path.clone(),
            digest: recovery.attachment.digest.clone(),
        },
    ]);
    Some(AcceptedRecheckResult {
        source_results: binding.source_results,
        command_index: binding.command_index,
        command: binding.command,
        comparison_commit: recheck.review_context.as_ref()?.base_commit.clone(),
        bindings,
    })
}

pub(crate) fn failed_command_recheck_correction_artifacts(
    project_root: &Path,
    recheck: &Task,
) -> Result<Vec<work_model::ArtifactRef>> {
    let area = recheck
        .artifact_area
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("failed-command recheck has no artifact area"))?;
    let manifest_path = validate_managed_file_path(
        project_root,
        &format!("{}/failed-command-recheck.json", area.path),
    )?;
    let manifest: crate::tester::FailedCommandRecheckManifest =
        serde_json::from_slice(&fs::read(&manifest_path)?)?;
    let results_path =
        validate_managed_file_path(project_root, &format!("{}/tester-results.json", area.path))?;
    let results: crate::tester::TesterResults = serde_json::from_slice(&fs::read(&results_path)?)?;
    let expected_commit = recheck
        .review_context
        .as_ref()
        .map(|context| context.candidate_commit.as_str())
        .ok_or_else(|| anyhow::anyhow!("failed-command recheck has no candidate context"))?;
    if results.candidate_commit.as_deref() != Some(expected_commit) {
        bail!(
            "failed-command recheck result is not bound to candidate {}",
            expected_commit
        );
    }
    let indexes_are_strict = manifest
        .failed_command_indexes
        .windows(2)
        .all(|indexes| indexes[0] < indexes[1]);
    let expected_source = recheck
        .input_artifacts
        .first()
        .map(|artifact| artifact.path.as_str());
    if manifest.schema_version != crate::tester::FAILED_COMMAND_RECHECK_SCHEMA_VERSION
        || manifest.candidate_commit != expected_commit
        || manifest.failed_command_indexes.len() != results.commands.len()
        || manifest.failed_command_indexes.is_empty()
        || !indexes_are_strict
        || expected_source != Some(manifest.source_results.as_str())
    {
        bail!("failed-command recheck manifest does not match its candidate and command results");
    }
    let structured_failure = results.summary.fail > 0 || results.error.is_some();
    let failed = results.commands.iter().enumerate().filter(|(_, command)| {
        structured_failure
            || command.exit_code != 0
            || command
                .process_settlement
                .as_ref()
                .is_some_and(|evidence| !evidence.leaks.is_empty())
    });
    let mut failed = failed.peekable();
    if !structured_failure && failed.peek().is_none() {
        return Ok(Vec::new());
    }

    let mut artifacts = vec![
        work_model::ArtifactRef {
            producer_id: recheck.id.clone(),
            path: format!("{}/failed-command-recheck.json", area.path),
        },
        work_model::ArtifactRef {
            producer_id: recheck.id.clone(),
            path: format!("{}/tester-results.json", area.path),
        },
    ];
    for (index, command) in failed {
        for (actual, expected) in [
            (&command.stdout_log, format!("commands/{index}-stdout.log")),
            (&command.stderr_log, format!("commands/{index}-stderr.log")),
        ] {
            if actual != &expected
                || validate_managed_file_path(project_root, &format!("{}/{}", area.path, expected))
                    .is_err()
            {
                bail!("failed-command recheck log path {actual:?} does not match {expected:?}");
            }
            artifacts.push(work_model::ArtifactRef {
                producer_id: recheck.id.clone(),
                path: format!("{}/{}", area.path, expected),
            });
        }
    }
    Ok(artifacts)
}
