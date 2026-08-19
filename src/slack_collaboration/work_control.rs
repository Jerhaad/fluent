//! Durable Slack Slice bindings and the narrow scheduler submission boundary.
//!
//! Plan approval writes proposed Work and its Slice binding under
//! `.fluent/work/slack/slices/`; execution approval is a separate action that
//! submits stable requests to the scheduler. Read methods project persisted
//! Work state, while approval and reconciliation methods mutate only their
//! durable bindings or the Work model.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::atomic_write::atomic_write;
use crate::work_model::{
    DurableTransitionMetadata, ExecutionAuthority, ExecutionAuthorization, WorkItem,
    WorkModelStore, validate_work_item_id,
};

/// A Work Item approved as part of one Slack Slice plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedWork {
    pub id: String,
    pub title: String,
}

impl PlannedWork {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
        }
    }
}

/// Stable execution request submitted to the scheduler-owned boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkExecutionRequest {
    pub version: u32,
    pub id: String,
    pub slice_id: String,
    pub work_item_id: String,
    pub authorized_by: String,
}

/// Scheduler acknowledgement that is safe to report in a Slack thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkQueueOutcome {
    Queued { request_id: String },
    AlreadyQueued { request_id: String },
}

/// Typed scheduler boundary. Implementations own queue storage and capacity;
/// Slack collaboration only submits a frozen request.
pub trait WorkScheduler {
    fn submit(&self, request: &WorkExecutionRequest) -> Result<WorkQueueOutcome>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedPlanReport {
    pub slice_id: String,
    pub work_item_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionApprovalReport {
    pub slice_id: String,
    pub outcomes: Vec<(String, WorkQueueOutcome)>,
}

/// Public, durable Work status rendered for a Slack thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SliceWorkStatus {
    pub work_item_id: String,
    pub state: String,
    pub next_action: String,
}

/// Projected Slice state. A completed Slice remains readable and can be linked
/// to future corrective Work without losing its original record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SliceStage {
    Active,
    Complete,
}

/// Durable Slice-to-Work binding, stored beside the bound project's Work model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SliceWork {
    pub id: String,
    pub approved_by: String,
    pub work_item_ids: Vec<String>,
    #[serde(default)]
    pub execution_requests: Vec<WorkExecutionRequest>,
    #[serde(default)]
    acknowledged_request_ids: Vec<String>,
    #[serde(default)]
    pub transitions: Vec<WorkTransition>,
    #[serde(default)]
    observed_states: BTreeMap<String, String>,
    #[serde(default)]
    transition_cursors: BTreeMap<String, u64>,
}

/// A bounded, ordered status change observed from the authoritative Work
/// aggregate. The originating actor stays optional because direct Fluent
/// mutations do not always carry one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkTransition {
    pub sequence: u64,
    pub work_item_id: String,
    #[serde(default)]
    pub object_id: String,
    pub old_state: Option<String>,
    pub new_state: String,
    pub origin: Option<String>,
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<String>,
    #[serde(default = "default_work_item_kind")]
    pub object_kind: String,
    #[serde(default)]
    pub occurred_at: Option<String>,
    #[serde(default)]
    pub elapsed_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison_base: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_tests: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_reviews: Vec<String>,
}

fn default_work_item_kind() -> String {
    "work-item".into()
}

/// Render one bounded, attributed transition reply for a Slack thread. The
/// caller supplies links only after it has verified that they are safe public
/// evidence references.
pub fn render_transition_reply(transition: &WorkTransition, evidence: &[String]) -> String {
    let object_id = if transition.object_id.is_empty() {
        &transition.work_item_id
    } else {
        &transition.object_id
    };
    let mut reply = format!(
        "{} {} for Work Item {} is now {}.",
        transition.object_kind, object_id, transition.work_item_id, transition.new_state
    );
    if let Some(origin) = &transition.origin {
        reply.push_str(&format!(" Origin: {origin}."));
    }
    if let Some(reason) = &transition.reason {
        reply.push_str(&format!(" Next action: {reason}."));
    }
    if let Some(elapsed) = transition.elapsed_seconds {
        reply.push_str(&format!(" Elapsed: {elapsed}s."));
    }
    if !evidence.is_empty() {
        reply.push_str(&format!(" Evidence: {}.", evidence.join(", ")));
    }
    reply
}

/// Materializes approved plans without starting execution, then independently
/// authorizes their stable scheduler requests.
pub struct WorkControl {
    project_root: PathBuf,
}

impl WorkControl {
    pub const REQUEST_VERSION: u32 = 1;

    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
        }
    }

    /// Materialize one approved plan as proposed Work without starting it.
    pub fn approve_plan(
        &self,
        slice_id: &str,
        approver: &str,
        planned_work: &[PlannedWork],
    ) -> Result<ApprovedPlanReport> {
        if slice_id.trim().is_empty() || approver.trim().is_empty() || planned_work.is_empty() {
            bail!("an approved plan requires a Slice, approver, and Work Items");
        }
        validate_slice_id(slice_id)?;
        let ids: Vec<_> = planned_work.iter().map(|work| work.id.clone()).collect();
        if ids.iter().any(|id| id.trim().is_empty())
            || planned_work.iter().any(|work| work.title.trim().is_empty())
            || ids.len() != ids.iter().collect::<std::collections::BTreeSet<_>>().len()
        {
            bail!("approved plan Work Items must have unique non-empty identifiers and titles");
        }
        if let Some(existing) = self.slice(slice_id)? {
            if existing.work_item_ids == ids && existing.approved_by == approver {
                return Ok(ApprovedPlanReport {
                    slice_id: existing.id,
                    work_item_ids: existing.work_item_ids,
                });
            }
            bail!("Slice {slice_id:?} already has a different approved plan");
        }

        let store = WorkModelStore::new(&self.project_root);
        for work in planned_work {
            let item = WorkItem {
                id: work.id.clone(),
                title: work.title.clone(),
                authorization: ExecutionAuthorization::Proposed,
                ..WorkItem::default()
            };
            store.create_work_item(&item).with_context(|| {
                format!("create planned Work Item {} for Slice {slice_id}", work.id)
            })?;
        }
        let slice = SliceWork {
            id: slice_id.into(),
            approved_by: approver.into(),
            work_item_ids: ids.clone(),
            execution_requests: Vec::new(),
            acknowledged_request_ids: Vec::new(),
            transitions: Vec::new(),
            observed_states: BTreeMap::new(),
            transition_cursors: BTreeMap::new(),
        };
        self.save_slice(&slice)?;
        Ok(ApprovedPlanReport {
            slice_id: slice_id.into(),
            work_item_ids: ids,
        })
    }

    pub fn authorize_execution(
        &self,
        slice_id: &str,
        approver: &str,
        scheduler: &dyn WorkScheduler,
    ) -> Result<ExecutionApprovalReport> {
        let mut slice = self
            .slice(slice_id)?
            .with_context(|| format!("approved Slice {slice_id} is unavailable"))?;
        if slice.approved_by != approver {
            bail!("only the approving project approver can authorize this Slice");
        }
        let store = WorkModelStore::new(&self.project_root);
        let mut outcomes = Vec::with_capacity(slice.work_item_ids.len());
        for work_item_id in &slice.work_item_ids {
            let existing_request = slice
                .execution_requests
                .iter()
                .find(|request| request.work_item_id == *work_item_id)
                .cloned();
            let request = existing_request
                .clone()
                .unwrap_or_else(|| WorkExecutionRequest {
                    version: Self::REQUEST_VERSION,
                    id: request_id(slice_id, work_item_id),
                    slice_id: slice_id.into(),
                    work_item_id: work_item_id.clone(),
                    authorized_by: approver.into(),
                });
            if existing_request.is_none() {
                slice.execution_requests.push(request.clone());
                self.save_slice(&slice)?;
            }
            let metadata = DurableTransitionMetadata {
                origin: Some("slack-thread".into()),
                reason: Some(format!("Execution approved by {approver}")),
                evidence_ref: Some(format!("slack-slice:{slice_id}")),
            };
            store.mutate_work_item_with_transition(work_item_id, Some(&metadata), |item| {
                item.authorization =
                    ExecutionAuthorization::execution_ready(ExecutionAuthority::Human);
                Ok(())
            })?;
            let outcome = if slice.acknowledged_request_ids.contains(&request.id) {
                WorkQueueOutcome::Queued {
                    request_id: request.id.clone(),
                }
            } else {
                // The scheduler owns idempotency. A persisted request is intent,
                // not proof of delivery: retry the same identity after a lost or
                // failed acknowledgement instead of reporting a synthetic queue.
                let outcome = scheduler.submit(&request)?;
                slice.acknowledged_request_ids.push(request.id.clone());
                self.save_slice(&slice)?;
                outcome
            };
            outcomes.push((work_item_id.clone(), outcome));
        }
        self.save_slice(&slice)?;
        Ok(ExecutionApprovalReport {
            slice_id: slice.id,
            outcomes,
        })
    }

    /// Read the persisted Slice binding without changing it.
    pub fn slice(&self, slice_id: &str) -> Result<Option<SliceWork>> {
        let path = self.slice_path(slice_id)?;
        match fs::read(&path) {
            Ok(bytes) => {
                Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                    format!("parse Slice Work {}", path.display())
                })?))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read Slice Work {}", path.display())),
        }
    }

    /// Read the authoritative persisted aggregate instead of inferring status
    /// from scheduler queues or prior Slack replies.
    pub fn status(&self, slice_id: &str) -> Result<Vec<SliceWorkStatus>> {
        let slice = self
            .slice(slice_id)?
            .with_context(|| format!("approved Slice {slice_id} is unavailable"))?;
        let store = WorkModelStore::new(&self.project_root);
        slice
            .work_item_ids
            .iter()
            .map(|work_item_id| {
                let item = store.read_work_item(work_item_id)?;
                let (state, next_action) = if item.authorization.is_proposed() {
                    ("planned", "Await execution approval")
                } else if let Some(attempt) = item.attempts.last() {
                    match attempt.status {
                        crate::work_model::AttemptStatus::NeedsUser => {
                            ("needs-user", "Respond to the current recovery request")
                        }
                        crate::work_model::AttemptStatus::Failed => {
                            ("failed", "Inspect durable evidence and permitted recovery")
                        }
                        crate::work_model::AttemptStatus::Complete => {
                            ("complete", "Await candidate or landing outcome")
                        }
                        crate::work_model::AttemptStatus::Planned => {
                            ("queued", "Await scheduler capacity")
                        }
                        crate::work_model::AttemptStatus::Executing => {
                            ("executing", "Await Attempt progress")
                        }
                        crate::work_model::AttemptStatus::Reviewing => {
                            ("reviewing", "Await review and Tester results")
                        }
                    }
                } else {
                    ("authorized", "Await scheduler dispatch")
                };
                Ok(SliceWorkStatus {
                    work_item_id: work_item_id.clone(),
                    state: state.into(),
                    next_action: next_action.into(),
                })
            })
            .collect()
    }

    /// Reconcile the persisted Work aggregate into an append-only Slice
    /// transition stream. A repeated poll after no Work mutation emits nothing.
    pub fn reconcile_transitions(
        &self,
        slice_id: &str,
        origin: Option<&str>,
    ) -> Result<Vec<WorkTransition>> {
        let mut slice = self
            .slice(slice_id)?
            .with_context(|| format!("approved Slice {slice_id} is unavailable"))?;
        let statuses = self.status(slice_id)?;
        let mut appended = Vec::new();
        for status in statuses {
            let old_state = slice.observed_states.get(&status.work_item_id).cloned();
            if old_state.as_deref() == Some(&status.state) {
                continue;
            }
            let transition = WorkTransition {
                sequence: slice.transitions.len() as u64 + 1,
                work_item_id: status.work_item_id.clone(),
                object_id: status.work_item_id.clone(),
                old_state,
                new_state: status.state.clone(),
                origin: origin.map(str::to_owned),
                reason: Some(status.next_action),
                evidence_ref: None,
                object_kind: "work-item".into(),
                occurred_at: None,
                elapsed_seconds: None,
                candidate_commit: None,
                comparison_base: None,
                completed_tests: Vec::new(),
                completed_reviews: Vec::new(),
            };
            slice
                .observed_states
                .insert(status.work_item_id, status.state);
            slice.transitions.push(transition.clone());
            appended.push(transition);
        }
        if !appended.is_empty() {
            self.save_slice(&slice)?;
        }
        Ok(appended)
    }

    /// Read the model-owned ledger after this Slack consumer's independent
    /// cursor. This reports transitions produced while the runner was away
    /// without conflating them with transitions another consumer has seen.
    pub fn reconcile_durable_transitions(&self, slice_id: &str) -> Result<Vec<WorkTransition>> {
        let mut slice = self
            .slice(slice_id)?
            .with_context(|| format!("approved Slice {slice_id} is unavailable"))?;
        let store = WorkModelStore::new(&self.project_root);
        let mut reported = Vec::new();
        for work_item_id in &slice.work_item_ids {
            let item = store.read_work_item(work_item_id)?;
            let cursor = slice
                .transition_cursors
                .get(work_item_id)
                .copied()
                .unwrap_or(0);
            for entry in item
                .transitions
                .iter()
                .filter(|entry| entry.sequence > cursor)
            {
                reported.push(WorkTransition {
                    sequence: entry.sequence,
                    work_item_id: work_item_id.clone(),
                    object_id: entry.object_id.clone(),
                    old_state: entry.old_state.clone(),
                    new_state: entry.new_state.clone(),
                    origin: entry.origin.clone(),
                    reason: entry.reason.clone(),
                    evidence_ref: entry.evidence_ref.clone(),
                    object_kind: entry.object_kind.clone(),
                    occurred_at: Some(entry.occurred_at.clone()),
                    elapsed_seconds: entry.elapsed_seconds,
                    candidate_commit: None,
                    comparison_base: None,
                    completed_tests: Vec::new(),
                    completed_reviews: Vec::new(),
                });
                slice
                    .transition_cursors
                    .insert(work_item_id.clone(), entry.sequence);
            }
        }
        reported.sort_by(|left, right| {
            left.sequence
                .cmp(&right.sequence)
                .then_with(|| left.work_item_id.cmp(&right.work_item_id))
        });
        if !reported.is_empty() {
            self.save_slice(&slice)?;
        }
        Ok(reported)
    }

    /// Schedule each unseen transition before advancing its durable cursor.
    /// Repeating after a crash is safe when `schedule` uses the transition's
    /// stable identity at an idempotent outbox boundary.
    pub fn schedule_durable_transitions(
        &self,
        slice_id: &str,
        mut schedule: impl FnMut(&WorkTransition) -> Result<()>,
    ) -> Result<Vec<WorkTransition>> {
        let mut slice = self
            .slice(slice_id)?
            .with_context(|| format!("approved Slice {slice_id} is unavailable"))?;
        let store = WorkModelStore::new(&self.project_root);
        let mut pending = Vec::new();
        for work_item_id in &slice.work_item_ids {
            let item = store.read_work_item(work_item_id)?;
            let cursor = slice
                .transition_cursors
                .get(work_item_id)
                .copied()
                .unwrap_or(0);
            pending.extend(
                item.transitions
                    .iter()
                    .filter(|entry| entry.sequence > cursor)
                    .map(|entry| WorkTransition {
                        sequence: entry.sequence,
                        work_item_id: work_item_id.clone(),
                        object_id: entry.object_id.clone(),
                        old_state: entry.old_state.clone(),
                        new_state: entry.new_state.clone(),
                        origin: entry.origin.clone(),
                        reason: entry.reason.clone(),
                        evidence_ref: entry.evidence_ref.clone(),
                        object_kind: entry.object_kind.clone(),
                        occurred_at: Some(entry.occurred_at.clone()),
                        elapsed_seconds: entry.elapsed_seconds,
                        candidate_commit: None,
                        comparison_base: None,
                        completed_tests: Vec::new(),
                        completed_reviews: Vec::new(),
                    }),
            );
        }
        pending.sort_by(|left, right| {
            left.occurred_at
                .cmp(&right.occurred_at)
                .then_with(|| left.work_item_id.cmp(&right.work_item_id))
                .then_with(|| left.sequence.cmp(&right.sequence))
        });
        for transition in &pending {
            schedule(transition)?;
            slice
                .transition_cursors
                .insert(transition.work_item_id.clone(), transition.sequence);
        }
        if !pending.is_empty() {
            self.save_slice(&slice)?;
        }
        Ok(pending)
    }

    /// Derive the current Slice stage from persisted Work and landing state.
    pub fn slice_stage(&self, slice_id: &str) -> Result<SliceStage> {
        let slice = self
            .slice(slice_id)?
            .with_context(|| format!("approved Slice {slice_id} is unavailable"))?;
        let store = WorkModelStore::new(&self.project_root);
        let mut completed = !slice.work_item_ids.is_empty();
        for work_item_id in &slice.work_item_ids {
            let item = store.read_work_item(work_item_id)?;
            let attempt_complete = matches!(
                item.attempts.last().map(|attempt| &attempt.status),
                Some(crate::work_model::AttemptStatus::Complete)
            );
            let candidates_landed = item.merge_candidates.iter().all(|candidate| {
                candidate.merge_state.status == crate::work_model::MergeCandidateMergeStatus::Merged
                    && candidate.merge_state.follow_up_failure.is_none()
            });
            completed &= attempt_complete && candidates_landed;
        }
        Ok(if completed {
            SliceStage::Complete
        } else {
            SliceStage::Active
        })
    }

    /// Render the pinned thread status from durable model state. The caller
    /// supplies provider state because it is runner-local rather than Work
    /// model authority.
    pub fn render_status_message(
        &self,
        slice_id: &str,
        project_label: &str,
        checkout_label: &str,
        provider_state: &str,
    ) -> Result<String> {
        let statuses = self.status(slice_id)?;
        let stage = self.slice_stage(slice_id)?;
        let work = statuses
            .iter()
            .map(|status| format!("{}: {}", status.work_item_id, status.state))
            .collect::<Vec<_>>()
            .join(", ");
        let next = statuses
            .first()
            .map(|status| status.next_action.as_str())
            .unwrap_or("No Work is planned");
        Ok(format!(
            "Project: {project_label}; checkout: {checkout_label}; Slice: {stage:?}; provider: {provider_state}; Work: {work}. Next action: {next}."
        ))
    }

    /// Explain a durable exceptional transition without publishing externally.
    pub fn render_actionable_transition(
        &self,
        transition: &WorkTransition,
        evidence: &[String],
        github_remote: Option<&str>,
    ) -> String {
        let mut reply = render_transition_reply(transition, evidence);
        match transition.new_state.as_str() {
            "needs-user" => reply.push_str(" Reply with the requested recovery decision."),
            "failed" => {
                reply.push_str(" Inspect the evidence and use only the permitted recovery action.")
            }
            "ready" => {
                reply.push_str(" Candidate is ready for review.");
                if let Some(commit) = &transition.candidate_commit {
                    reply.push_str(&format!(" Commit: {commit}."));
                    if !transition.completed_tests.is_empty() {
                        reply.push_str(&format!(
                            " Tests: {}.",
                            transition.completed_tests.join(", ")
                        ));
                    }
                    if !transition.completed_reviews.is_empty() {
                        reply.push_str(&format!(
                            " Reviews: {}.",
                            transition.completed_reviews.join(", ")
                        ));
                    }
                    if let Some(remote) = github_remote {
                        reply.push_str(&format!(" Review {remote}/commit/{commit}."));
                        if let Some(base) = &transition.comparison_base {
                            reply
                                .push_str(&format!(" Compare {remote}/compare/{base}...{commit}."));
                        }
                        reply.push_str(" Optionally create a pull request after review.");
                    }
                }
            }
            _ => {}
        }
        reply
    }

    /// Preserve a completed Slice and offer an explicit linked conversation for
    /// newly requested product work. This does not create or authorize Work.
    pub fn linked_slice_offer(
        &self,
        completed_slice_id: &str,
        requested_work: &str,
    ) -> Result<String> {
        if self.slice_stage(completed_slice_id)? != SliceStage::Complete {
            bail!("Slice {completed_slice_id:?} is not complete");
        }
        let mut hasher = Sha256::new();
        hasher.update(b"fluent-slack-linked-slice-v1\0");
        hasher.update(completed_slice_id.as_bytes());
        hasher.update([0]);
        hasher.update(requested_work.as_bytes());
        Ok(format!(
            "Slice {completed_slice_id} remains complete. Start linked Slice slice-{:x} for the requested product Work?",
            hasher.finalize()
        ))
    }

    /// Report an optional post-land Observation without changing Slice stage.
    pub fn render_optional_observation(
        &self,
        slice_id: &str,
        observation_ref: &str,
    ) -> Result<String> {
        if self.slice_stage(slice_id)? != SliceStage::Complete {
            bail!("optional Observations do not close an active Slice");
        }
        Ok(format!(
            "Slice {slice_id} remains complete. Observation: {observation_ref}."
        ))
    }

    fn save_slice(&self, slice: &SliceWork) -> Result<()> {
        let path = self.slice_path(&slice.id)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, &serde_json::to_vec_pretty(slice)?)?;
        Ok(())
    }

    fn slice_path(&self, slice_id: &str) -> Result<PathBuf> {
        validate_slice_id(slice_id)?;
        Ok(self
            .project_root
            .join(".fluent/work/slack/slices")
            .join(format!("{slice_id}.json")))
    }
}

fn validate_slice_id(slice_id: &str) -> Result<()> {
    validate_work_item_id(slice_id).map_err(Into::into)
}

fn request_id(slice_id: &str, work_item_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"fluent-slack-work-request-v1\0");
    hasher.update(slice_id.as_bytes());
    hasher.update([0]);
    hasher.update(work_item_id.as_bytes());
    format!("slack-work-{:x}", hasher.finalize())
}
