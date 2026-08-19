use crate::atomic_write::atomic_write;
use crate::work_model::{DurableWorkTransition, WorkModelStore};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

const PROJECTION_SCHEMA_VERSION: u32 = 1;

/// One Work transition projected by the dashboard's independent consumer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DashboardTransition {
    pub work_item_id: String,
    pub transition: DurableWorkTransition,
}

/// The dashboard's durable latest-transition view and newly applied entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DashboardTransitionState {
    pub latest: BTreeMap<String, DashboardTransition>,
    pub applied: Vec<DashboardTransition>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredProjection {
    schema_version: u32,
    #[serde(default)]
    cursors: BTreeMap<String, u64>,
    #[serde(default)]
    latest: BTreeMap<String, DashboardTransition>,
}

impl Default for StoredProjection {
    fn default() -> Self {
        Self {
            schema_version: PROJECTION_SCHEMA_VERSION,
            cursors: BTreeMap::new(),
            latest: BTreeMap::new(),
        }
    }
}

/// Consume Work ledgers without reading or mutating Slack Slice state.
pub(crate) struct DashboardTransitionProjection {
    project_root: PathBuf,
}

impl DashboardTransitionProjection {
    pub(crate) fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }

    /// Apply every entry after the dashboard cursor and acknowledge it only
    /// when the updated cursor and latest-transition projection are durable.
    pub(crate) fn refresh(&self) -> Result<DashboardTransitionState> {
        let path = self.path();
        let mut stored = self.read()?;
        let mut applied = Vec::new();
        for item in WorkModelStore::new(&self.project_root).list_work_items()? {
            let cursor = stored.cursors.get(&item.id).copied().unwrap_or(0);
            for transition in item
                .transitions
                .iter()
                .filter(|transition| transition.sequence > cursor)
            {
                let projected = DashboardTransition {
                    work_item_id: item.id.clone(),
                    transition: transition.clone(),
                };
                stored.latest.insert(item.id.clone(), projected.clone());
                stored.cursors.insert(item.id.clone(), transition.sequence);
                applied.push(projected);
            }
        }
        applied.sort_by(|left, right| {
            left.transition
                .occurred_at
                .cmp(&right.transition.occurred_at)
                .then_with(|| left.work_item_id.cmp(&right.work_item_id))
                .then_with(|| left.transition.sequence.cmp(&right.transition.sequence))
        });
        if !applied.is_empty() {
            let parent = path
                .parent()
                .context("dashboard transition projection has no parent directory")?;
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "create dashboard transition projection directory at {}",
                    parent.display()
                )
            })?;
            atomic_write(&path, &serde_json::to_vec_pretty(&stored)?).with_context(|| {
                format!(
                    "write dashboard transition projection at {}",
                    path.display()
                )
            })?;
        }
        Ok(DashboardTransitionState {
            latest: stored.latest,
            applied,
        })
    }

    fn path(&self) -> PathBuf {
        self.project_root
            .join(".fluent/work/dashboard/transition-projection.json")
    }

    fn read(&self) -> Result<StoredProjection> {
        let path = self.path();
        if !path.exists() {
            return Ok(StoredProjection::default());
        }
        let stored: StoredProjection =
            serde_json::from_slice(&fs::read(&path).with_context(|| {
                format!("read dashboard transition projection at {}", path.display())
            })?)
            .with_context(|| {
                format!(
                    "parse dashboard transition projection at {}",
                    path.display()
                )
            })?;
        if stored.schema_version != PROJECTION_SCHEMA_VERSION {
            bail!(
                "unsupported dashboard transition projection version {} at {}",
                stored.schema_version,
                path.display()
            );
        }
        Ok(stored)
    }
}
