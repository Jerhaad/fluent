use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::atomic_write::atomic_write;
use crate::work_model::{ExecutionAuthority, ExecutionAuthorization, WorkItem, WorkModelStore};

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

/// Durable Slice-to-Work binding, stored beside the bound project's Work model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SliceWork {
    pub id: String,
    pub approved_by: String,
    pub work_item_ids: Vec<String>,
    #[serde(default)]
    pub execution_requests: Vec<WorkExecutionRequest>,
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

    pub fn approve_plan(
        &self,
        slice_id: &str,
        approver: &str,
        planned_work: &[PlannedWork],
    ) -> Result<ApprovedPlanReport> {
        if slice_id.trim().is_empty() || approver.trim().is_empty() || planned_work.is_empty() {
            bail!("an approved plan requires a Slice, approver, and Work Items");
        }
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
            store.mutate_work_item(work_item_id, |item| {
                item.authorization =
                    ExecutionAuthorization::execution_ready(ExecutionAuthority::Human);
                Ok(())
            })?;
            let outcome = match existing_request {
                Some(_) => WorkQueueOutcome::Queued {
                    request_id: request.id.clone(),
                },
                None => scheduler.submit(&request)?,
            };
            outcomes.push((work_item_id.clone(), outcome));
        }
        self.save_slice(&slice)?;
        Ok(ExecutionApprovalReport {
            slice_id: slice.id,
            outcomes,
        })
    }

    pub fn slice(&self, slice_id: &str) -> Result<Option<SliceWork>> {
        let path = self.slice_path(slice_id);
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

    fn save_slice(&self, slice: &SliceWork) -> Result<()> {
        let path = self.slice_path(&slice.id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, &serde_json::to_vec_pretty(slice)?)?;
        Ok(())
    }

    fn slice_path(&self, slice_id: &str) -> PathBuf {
        self.project_root
            .join(".fluent/work/slack/slices")
            .join(format!("{slice_id}.json"))
    }
}

fn request_id(slice_id: &str, work_item_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"fluent-slack-work-request-v1\0");
    hasher.update(slice_id.as_bytes());
    hasher.update([0]);
    hasher.update(work_item_id.as_bytes());
    format!("slack-work-{:x}", hasher.finalize())
}
