//! Classify immutable Tester results and failed-command recheck artifacts.

use anyhow::{Result, bail};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::work_model::{ArtifactRef, Attempt, Task, work_artifact_path};

pub(crate) fn tester_failures(tester_results_path: &Path) -> usize {
    let content = match fs::read_to_string(tester_results_path) {
        Ok(content) => content,
        Err(_) => return 0,
    };
    let results: crate::tester::TesterResults = match serde_json::from_str(&content) {
        Ok(results) => results,
        Err(_) => return 0,
    };
    results
        .tests
        .iter()
        .filter(|test| test.status == "fail")
        .count()
}

pub(crate) fn failing_ids(path: &Path) -> HashSet<String> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => return HashSet::new(),
    };
    let results: crate::tester::TesterResults = match serde_json::from_str(&content) {
        Ok(results) => results,
        Err(_) => return HashSet::new(),
    };
    results
        .tests
        .iter()
        .filter(|test| test.status == "fail")
        .map(|test| test.id.clone())
        .collect()
}

pub(crate) fn introduced_tester_failures(
    current_path: &Path,
    baseline_path: Option<&Path>,
) -> usize {
    match baseline_path {
        Some(baseline_path) => {
            let current = failing_ids(current_path);
            let baseline = failing_ids(baseline_path);
            current.difference(&baseline).count()
        }
        None => tester_failures(current_path),
    }
}

pub(crate) fn introduced_tester_failures_except_command(
    current_path: &Path,
    baseline_path: Option<&Path>,
    command_index: usize,
    command: &str,
) -> usize {
    let mut current = failing_ids(current_path);
    current.remove(&format!("command-{command_index}:{command}"));
    match baseline_path {
        Some(baseline_path) => {
            let baseline = failing_ids(baseline_path);
            current.difference(&baseline).count()
        }
        None => current.len(),
    }
}

pub(crate) fn unexplained_failed_tester_commands(
    tester_results_path: &Path,
    baseline_path: Option<&Path>,
) -> usize {
    let current: crate::tester::TesterResults = match fs::read(tester_results_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<crate::tester::TesterResults>(&bytes).ok())
    {
        Some(results) => results,
        None => return 0,
    };
    let failed_count = current
        .commands
        .iter()
        .filter(|command| command.exit_code != 0)
        .count();
    if failed_count == 0 {
        return 0;
    }
    let baseline: crate::tester::TesterResults = match baseline_path
        .and_then(|path| fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<crate::tester::TesterResults>(&bytes).ok())
    {
        Some(results) if current.error.is_none() && results.error.is_none() => results,
        _ => return failed_count,
    };

    current
        .commands
        .iter()
        .enumerate()
        .filter(|(index, command)| {
            if command.exit_code == 0 {
                return false;
            }
            let Some(baseline_command) = baseline.commands.get(*index) else {
                return true;
            };
            let command_identity_matches = command.command == baseline_command.command
                && command.test_harness == baseline_command.test_harness
                && command.exit_code == baseline_command.exit_code;
            let settlements_are_clean = [command, baseline_command].iter().all(|command| {
                command
                    .process_settlement
                    .as_ref()
                    .is_none_or(|settlement| settlement.leaks.is_empty())
            });
            let harness_is_unambiguous = current
                .commands
                .iter()
                .filter(|candidate| {
                    candidate.exit_code != 0 && candidate.test_harness == command.test_harness
                })
                .count()
                == 1
                && baseline
                    .commands
                    .iter()
                    .filter(|candidate| {
                        candidate.exit_code != 0 && candidate.test_harness == command.test_harness
                    })
                    .count()
                    == 1;
            let current_failures = current
                .tests
                .iter()
                .filter(|test| test.status == "fail" && test.test_harness == command.test_harness)
                .map(|test| test.id.as_str())
                .collect::<HashSet<_>>();
            let baseline_failures = baseline
                .tests
                .iter()
                .filter(|test| test.status == "fail" && test.test_harness == command.test_harness)
                .map(|test| test.id.as_str())
                .collect::<HashSet<_>>();
            !(command_identity_matches
                && settlements_are_clean
                && harness_is_unambiguous
                && !current_failures.is_empty()
                && current_failures.is_subset(&baseline_failures))
        })
        .count()
}

pub(crate) fn baseline_tester_results_path(
    project_root: &Path,
    attempt: &Attempt,
) -> Option<PathBuf> {
    let artifact = format!("{}-baseline-tester", attempt.id);
    let path = project_root
        .join(work_artifact_path(
            &attempt.work_item_id,
            &attempt.id,
            &artifact,
        ))
        .join("tester-results.json");
    path.is_file().then_some(path)
}

pub(crate) fn failed_command_recheck_correction_artifacts(
    project_root: &Path,
    recheck: &Task,
) -> Result<Vec<ArtifactRef>> {
    let area = recheck
        .artifact_area
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("failed-command recheck has no artifact area"))?;
    let artifact_dir =
        crate::work_task_executor::resolve_managed_artifact_area_path(project_root, &area.path)?;
    let manifest_path = artifact_dir.join("failed-command-recheck.json");
    let manifest: crate::tester::FailedCommandRecheckManifest =
        serde_json::from_slice(&fs::read(&manifest_path)?)?;
    let results_path = artifact_dir.join("tester-results.json");
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
        ArtifactRef {
            producer_id: recheck.id.clone(),
            path: format!("{}/failed-command-recheck.json", area.path),
        },
        ArtifactRef {
            producer_id: recheck.id.clone(),
            path: format!("{}/tester-results.json", area.path),
        },
    ];
    for (index, command) in failed {
        for (actual, expected) in [
            (&command.stdout_log, format!("commands/{index}-stdout.log")),
            (&command.stderr_log, format!("commands/{index}-stderr.log")),
        ] {
            if actual != &expected || !artifact_dir.join(&expected).is_file() {
                bail!("failed-command recheck log path {actual:?} does not match {expected:?}");
            }
            artifacts.push(ArtifactRef {
                producer_id: recheck.id.clone(),
                path: format!("{}/{}", area.path, expected),
            });
        }
    }
    Ok(artifacts)
}
