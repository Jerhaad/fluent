//! Offline replay checks for the recovery failures that have occurred in real
//! Fluent runs.
//!
//! The corpus deliberately stores a small description of each incident rather
//! than a copy of a whole project checkout. Each case still has the identity
//! fields that a public recovery must bind, the command that an operator should
//! be able to run, and the history that must remain visible. Attempt cases are
//! replayed through the same shared recovery-operation validator used by the
//! production recovery modules. This keeps the checks deterministic and
//! provider-free while still exercising candidate binding, stale-input
//! rejection, exact replay, and history preservation.

use crate::work_model::{
    AttemptReviewState, AttemptStatus, PauseKind, RecoveryOperationRequest, Task, TaskArtifactArea,
    TaskKind, TaskOutput, TaskStatus, WorkItem, WorkspaceAccess, initial_candidate_workspace_path,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::path::Path;

pub const RECOVERY_REPLAY_SCHEMA_VERSION: u32 = 1;

const CORPUS: &str = include_str!("../tests/fixtures/recovery-corpus.json");
const APPROACH_REVISION_ROUND_CAP_OPERATION_KIND: &str = "approach-revision-round-cap";

/// The scope of a saved incident. Attempt cases exercise the shared recovery
/// operation record; workflow cases document a public command whose state is
/// reconciled outside an Attempt (for example, an already-landed candidate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReplayScope {
    Attempt,
    Workflow,
}

impl ReplayScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attempt => "attempt",
            Self::Workflow => "workflow",
        }
    }
}

/// One sanitized, durable description of an observed recovery failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RecoveryReplayCase {
    pub id: String,
    pub incident: String,
    pub scope: ReplayScope,
    pub work_item_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_kind: Option<String>,
    pub operation_kind: String,
    /// The complete public command shown to the operator. Angle-bracket values
    /// are intentional placeholders, never paths from the original incident.
    pub public_command: String,
    pub candidate_commit: String,
    pub stale_candidate_commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<String>,
    /// Logical history labels retained by the real recovery. These are labels,
    /// not copied artifact paths, so the corpus contains no machine-local data.
    #[serde(default)]
    pub preserved_history: Vec<String>,
}

/// The versioned replay corpus checked into the Fluent repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RecoveryReplayCorpus {
    pub schema_version: u32,
    pub cases: Vec<RecoveryReplayCase>,
}

/// One successful offline replay result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReplayResult {
    pub case_id: String,
    pub operation_id: Option<String>,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayCorpusError(String);

impl ReplayCorpusError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ReplayCorpusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ReplayCorpusError {}

/// Parse and validate the repository's checked-in corpus.
pub fn load_corpus() -> Result<RecoveryReplayCorpus, ReplayCorpusError> {
    let corpus: RecoveryReplayCorpus = serde_json::from_str(CORPUS).map_err(|error| {
        ReplayCorpusError::new(format!("recovery corpus is invalid JSON: {error}"))
    })?;
    validate_corpus(&corpus)?;
    Ok(corpus)
}

/// Validate corpus shape without executing any provider or shell command.
pub fn validate_corpus(corpus: &RecoveryReplayCorpus) -> Result<(), ReplayCorpusError> {
    if corpus.schema_version != RECOVERY_REPLAY_SCHEMA_VERSION {
        return Err(ReplayCorpusError::new(format!(
            "recovery corpus schema version {} is unsupported (expected {})",
            corpus.schema_version, RECOVERY_REPLAY_SCHEMA_VERSION
        )));
    }
    if corpus.cases.is_empty() {
        return Err(ReplayCorpusError::new(
            "recovery corpus must contain at least one case",
        ));
    }

    let mut ids = HashSet::new();
    for case in &corpus.cases {
        if case.id.trim().is_empty() || !ids.insert(case.id.as_str()) {
            return Err(ReplayCorpusError::new(format!(
                "recovery corpus case id {:?} is empty or duplicated",
                case.id
            )));
        }
        if case.incident.trim().is_empty() {
            return Err(ReplayCorpusError::new(format!(
                "recovery corpus case {} has no incident description",
                case.id
            )));
        }
        if case.work_item_id.trim().is_empty() || case.work_item_id.contains('/') {
            return Err(ReplayCorpusError::new(format!(
                "recovery corpus case {} has an unsafe Work Item id",
                case.id
            )));
        }
        if case.operation_kind.trim().is_empty() || case.operation_kind.len() > 128 {
            return Err(ReplayCorpusError::new(format!(
                "recovery corpus case {} has an invalid operation kind",
                case.id
            )));
        }
        let command = case.public_command.trim();
        if !command.starts_with("fluent ") {
            return Err(ReplayCorpusError::new(format!(
                "recovery corpus case {} does not name a Fluent command",
                case.id
            )));
        }
        if !command.contains(&case.work_item_id) {
            return Err(ReplayCorpusError::new(format!(
                "recovery corpus case {} command is not bound to its Work Item",
                case.id
            )));
        }
        for forbidden in [
            "/Users/",
            "/home/",
            "BEGIN OPENSSH",
            "api_key",
            "API_KEY",
            "sk-",
        ] {
            if case.incident.contains(forbidden)
                || case.public_command.contains(forbidden)
                || case
                    .authorization
                    .as_deref()
                    .is_some_and(|value| value.contains(forbidden))
            {
                return Err(ReplayCorpusError::new(format!(
                    "recovery corpus case {} contains machine-local or secret data",
                    case.id
                )));
            }
        }
        if case.candidate_commit.trim().is_empty()
            || case.stale_candidate_commit.trim().is_empty()
            || case.candidate_commit == case.stale_candidate_commit
        {
            return Err(ReplayCorpusError::new(format!(
                "recovery corpus case {} must contain distinct current and stale candidates",
                case.id
            )));
        }
        if case.preserved_history.is_empty()
            || case
                .preserved_history
                .iter()
                .any(|entry| entry.trim().is_empty())
        {
            return Err(ReplayCorpusError::new(format!(
                "recovery corpus case {} must name preserved history",
                case.id
            )));
        }
        validate_approach_revision_case(case)?;

        match case.scope {
            ReplayScope::Attempt => {
                let attempt_id = case.attempt_id.as_deref().ok_or_else(|| {
                    ReplayCorpusError::new(format!(
                        "Attempt replay case {} has no Attempt id",
                        case.id
                    ))
                })?;
                if attempt_id.trim().is_empty() || !case.public_command.contains(attempt_id) {
                    return Err(ReplayCorpusError::new(format!(
                        "Attempt replay case {} command is not bound to its Attempt",
                        case.id
                    )));
                }
                if case.pause_kind.as_deref().is_none_or(str::is_empty) {
                    return Err(ReplayCorpusError::new(format!(
                        "Attempt replay case {} has no pause kind",
                        case.id
                    )));
                }
                if case.authorization.as_deref().is_none_or(str::is_empty) {
                    return Err(ReplayCorpusError::new(format!(
                        "Attempt replay case {} has no human authorization",
                        case.id
                    )));
                }
            }
            ReplayScope::Workflow => {
                if case.pause_kind.is_some() {
                    return Err(ReplayCorpusError::new(format!(
                        "workflow replay case {} must not claim an Attempt pause",
                        case.id
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_approach_revision_case(case: &RecoveryReplayCase) -> Result<(), ReplayCorpusError> {
    if case.operation_kind != APPROACH_REVISION_ROUND_CAP_OPERATION_KIND {
        return Ok(());
    }

    if case.scope != ReplayScope::Attempt
        || case.pause_kind.as_deref() != Some("round-cap")
        || !case.public_command.contains("attempt revise-approach")
        || !case.public_command.contains("--approach-file")
        || !case.public_command.contains("--resolution")
        || case.input_digest.as_deref().is_none_or(str::is_empty)
        || case.authorization.as_deref().is_none_or(str::is_empty)
    {
        return Err(ReplayCorpusError::new(format!(
            "recovery corpus case {} does not describe an explicitly authorized round-cap Approach revision",
            case.id
        )));
    }

    for required in [
        "round-cap-extension",
        "prior-approach",
        "replacement-approach",
        "settled-reviews",
        "corrective-writer",
    ] {
        if !case.preserved_history.iter().any(|entry| entry == required) {
            return Err(ReplayCorpusError::new(format!(
                "recovery corpus case {} omits preserved Approach revision history {required:?}",
                case.id
            )));
        }
    }

    Ok(())
}

/// Replay every case through the offline checks and return the stable result
/// identities. This is intentionally read-only and never invokes a coder.
pub fn replay_corpus() -> Result<Vec<RecoveryReplayResult>, ReplayCorpusError> {
    let corpus = load_corpus()?;
    corpus
        .cases
        .iter()
        .map(replay_case)
        .collect::<Result<Vec<_>, _>>()
}

fn replay_case(case: &RecoveryReplayCase) -> Result<RecoveryReplayResult, ReplayCorpusError> {
    match case.scope {
        ReplayScope::Workflow => Ok(RecoveryReplayResult {
            case_id: case.id.clone(),
            operation_id: None,
            command: case.public_command.clone(),
        }),
        ReplayScope::Attempt => replay_attempt_case(case),
    }
}

fn replay_attempt_case(
    case: &RecoveryReplayCase,
) -> Result<RecoveryReplayResult, ReplayCorpusError> {
    let attempt_id = case.attempt_id.as_deref().ok_or_else(|| {
        ReplayCorpusError::new(format!("Attempt replay case {} has no Attempt id", case.id))
    })?;
    let mut item = WorkItem::planned(&case.work_item_id, &case.incident);
    item.add_initial_attempt(attempt_id).map_err(|error| {
        ReplayCorpusError::new(format!(
            "recovery corpus case {} could not create its model fixture: {error}",
            case.id
        ))
    })?;
    let attempt = item
        .attempts
        .iter_mut()
        .find(|attempt| attempt.id == attempt_id)
        .ok_or_else(|| ReplayCorpusError::new("recovery corpus fixture lost its Attempt"))?;
    attempt.status = AttemptStatus::NeedsUser;
    attempt.review_state = Some(AttemptReviewState::Failed);
    let pause_kind = case
        .pause_kind
        .as_deref()
        .ok_or_else(|| ReplayCorpusError::new("Attempt replay case has no pause kind"))?;
    attempt.pause_kind = Some(parse_pause_kind(pause_kind).ok_or_else(|| {
        ReplayCorpusError::new(format!(
            "recovery corpus case {} has an unknown pause kind",
            case.id
        ))
    })?);
    let writer = attempt
        .tasks
        .first_mut()
        .ok_or_else(|| ReplayCorpusError::new("recovery corpus fixture has no initial Writer"))?;
    writer.status = crate::work_model::TaskStatus::Complete;
    writer.output = Some(TaskOutput {
        workspace_id: "candidate".to_string(),
        workspace_path: initial_candidate_workspace_path(&case.work_item_id, attempt_id),
        source_branch: format!("replay/{}/{}", case.work_item_id, attempt_id),
        base_commit: Some(case.stale_candidate_commit.clone()),
        commit: case.candidate_commit.clone(),
        no_change: None,
        learner_canonicalization: None,
    });
    if let Some(task_id) = case.task_id.as_deref()
        && task_id != writer.id
    {
        attempt.tasks.push(Task {
            id: task_id.to_string(),
            kind: TaskKind::Tester,
            status: TaskStatus::Failed,
            role: "replay-history".to_string(),
            instructions: None,
            work_item_id: case.work_item_id.clone(),
            attempt_id: Some(attempt_id.to_string()),
            workspace_access: WorkspaceAccess {
                reads: Vec::new(),
                writes: Vec::new(),
            },
            artifact_area: Some(TaskArtifactArea {
                path: format!(
                    ".fluent/work/artifacts/{}/{}/replay-history",
                    case.work_item_id, attempt_id
                ),
            }),
            review_context: None,
            writer_correction_assignment: None,
            verification_context: None,
            evidence_review_context: None,
            input_artifacts: Vec::new(),
            depends_on: None,
            output: None,
            created_at: None,
            started_at: None,
            completed_at: None,
        });
    }

    let request = RecoveryOperationRequest::new(
        &case.operation_kind,
        &case.work_item_id,
        attempt_id,
        Some(case.candidate_commit.clone()),
        case.task_id.clone(),
        case.evidence_digest.clone(),
        None,
        case.authorization.clone(),
        case.input_digest.clone(),
    );
    let project_root = Path::new(".");
    if item
        .validate_recovery_operation(project_root, &request)
        .map_err(|error| ReplayCorpusError::new(error.to_string()))?
        .is_some()
    {
        return Err(ReplayCorpusError::new(format!(
            "recovery corpus case {} unexpectedly started with a prior operation",
            case.id
        )));
    }
    let task_count_before_record = item.attempts[0].tasks.len();
    let operation = item
        .attempts
        .first_mut()
        .ok_or_else(|| ReplayCorpusError::new("recovery corpus fixture lost its Attempt"))?
        .record_recovery_operation(&request, Some(case.public_command.clone()))
        .map_err(|error| ReplayCorpusError::new(error.to_string()))?;
    if operation.next_action.as_deref() != Some(case.public_command.as_str()) {
        return Err(ReplayCorpusError::new(format!(
            "recovery corpus case {} did not retain its public command",
            case.id
        )));
    }

    let replayed = item
        .validate_recovery_operation(project_root, &request)
        .map_err(|error| ReplayCorpusError::new(error.to_string()))?
        .ok_or_else(|| ReplayCorpusError::new("exact recovery replay did not return its record"))?;
    if replayed != operation {
        return Err(ReplayCorpusError::new(format!(
            "recovery corpus case {} changed its operation on replay",
            case.id
        )));
    }

    let stale_request = RecoveryOperationRequest::new(
        &case.operation_kind,
        &case.work_item_id,
        attempt_id,
        Some(case.stale_candidate_commit.clone()),
        case.task_id.clone(),
        case.evidence_digest.clone(),
        None,
        case.authorization.clone(),
        case.input_digest.clone(),
    );
    if item
        .validate_recovery_operation(project_root, &stale_request)
        .is_ok()
    {
        return Err(ReplayCorpusError::new(format!(
            "recovery corpus case {} accepted a stale candidate",
            case.id
        )));
    }
    if item.attempts[0].tasks.len() != task_count_before_record
        || item.attempts[0].recovery_operations.len() != 1
    {
        return Err(ReplayCorpusError::new(format!(
            "recovery corpus case {} did not preserve history during replay",
            case.id
        )));
    }
    let encoded = serde_json::to_string(&item).map_err(|error| {
        ReplayCorpusError::new(format!("fixture serialization failed: {error}"))
    })?;
    let round_trip: WorkItem = serde_json::from_str(&encoded).map_err(|error| {
        ReplayCorpusError::new(format!("fixture deserialization failed: {error}"))
    })?;
    if round_trip.attempts[0].recovery_operations != item.attempts[0].recovery_operations {
        return Err(ReplayCorpusError::new(format!(
            "recovery corpus case {} lost its operation record on serialization",
            case.id
        )));
    }

    Ok(RecoveryReplayResult {
        case_id: case.id.clone(),
        operation_id: Some(operation.operation_id),
        command: case.public_command.clone(),
    })
}

fn parse_pause_kind(value: &str) -> Option<PauseKind> {
    Some(match value.trim() {
        "auth" => PauseKind::Auth,
        "uncertain" => PauseKind::Uncertain,
        "writer-completion" => PauseKind::WriterCompletion,
        "candidate-gate" => PauseKind::CandidateGate,
        "host-evidence" => PauseKind::HostEvidence,
        "round-cap" => PauseKind::RoundCap,
        "transcript-pump" => PauseKind::TranscriptPump,
        "host-sandbox" => PauseKind::HostSandbox,
        "tester-harness" => PauseKind::TesterHarness,
        "provider-unavailable" => PauseKind::ProviderUnavailable,
        "interrupted" => PauseKind::Interrupted,
        _ => return None,
    })
}

impl std::str::FromStr for ReplayScope {
    type Err = ReplayCorpusError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "attempt" => Ok(Self::Attempt),
            "workflow" => Ok(Self::Workflow),
            _ => Err(ReplayCorpusError::new(format!(
                "unknown replay scope {value:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_corpus_is_valid_and_replayable() {
        let corpus = load_corpus().expect("the checked-in recovery corpus is valid");
        assert!(corpus.cases.len() >= 8);
        let results = replay_corpus().expect("every recovery case replays offline");
        assert_eq!(results.len(), corpus.cases.len());
        assert!(
            results
                .iter()
                .all(|result| result.command.starts_with("fluent "))
        );
    }

    #[test]
    fn recovery_corpus_rejects_duplicate_ids_and_future_schema() {
        let corpus = load_corpus().unwrap();
        let mut duplicate = corpus.clone();
        duplicate.cases.push(duplicate.cases[0].clone());
        assert!(validate_corpus(&duplicate).is_err());

        let mut future = corpus;
        future.schema_version = RECOVERY_REPLAY_SCHEMA_VERSION + 1;
        assert!(validate_corpus(&future).is_err());
    }

    #[test]
    fn round_cap_approach_revision_replay_requires_its_bound_history() {
        let corpus = load_corpus().unwrap();
        let case = corpus
            .cases
            .iter()
            .find(|case| case.id == "approach-revision-round-cap")
            .expect("the sanitized Approach revision replay case exists");
        assert_eq!(case.pause_kind.as_deref(), Some("round-cap"));
        assert_eq!(case.operation_kind, "approach-revision-round-cap");

        let mut missing_history = corpus;
        let replay = missing_history
            .cases
            .iter_mut()
            .find(|case| case.id == "approach-revision-round-cap")
            .unwrap();
        replay
            .preserved_history
            .retain(|entry| entry != "replacement-approach");
        assert!(validate_corpus(&missing_history).is_err());
    }

    #[test]
    fn recovery_corpus_scope_names_are_stable_for_serialized_fixtures() {
        assert_eq!(ReplayScope::Attempt.as_str(), "attempt");
        assert_eq!(ReplayScope::Workflow.as_str(), "workflow");
        assert_eq!(
            "attempt".parse::<ReplayScope>().unwrap(),
            ReplayScope::Attempt
        );
    }
}
