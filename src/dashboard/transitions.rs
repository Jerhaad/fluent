use crate::work_model::{DurableWorkTransition, WorkItem};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One Work transition projected from the published ledger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DashboardTransition {
    pub work_item_id: String,
    pub transition: DurableWorkTransition,
}

/// Derive the latest transition without persisting a dashboard cursor.
pub(crate) fn latest_transitions(items: &[WorkItem]) -> BTreeMap<String, DashboardTransition> {
    items
        .iter()
        .filter_map(|item| {
            item.transitions.last().map(|transition| {
                (
                    item.id.clone(),
                    DashboardTransition {
                        work_item_id: item.id.clone(),
                        transition: transition.clone(),
                    },
                )
            })
        })
        .collect()
}
