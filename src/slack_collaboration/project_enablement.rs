use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::checkout_identity;

/// One runner-owner-approved directory that may contain project checkouts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDiscoveryRoot {
    pub label: String,
    pub path: PathBuf,
    pub maximum_depth: usize,
    pub allowed_participants: BTreeSet<String>,
    pub project_approvers: BTreeSet<String>,
}

/// A path-free checkout choice safe to render in Slack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCheckoutOffer {
    pub selection_id: String,
    pub project_label: String,
    pub checkout_label: String,
}

#[derive(Debug, Clone)]
struct DiscoveredCheckout {
    offer: ProjectCheckoutOffer,
    path: PathBuf,
    identity: String,
    approvers: BTreeSet<String>,
}

/// A point-in-time, owner-bounded discovery result. Public offers contain no
/// host paths; later mutations resolve through the private canonical entries.
#[derive(Debug, Clone, Default)]
pub struct ProjectDiscoveryIndex {
    entries: BTreeMap<String, DiscoveredCheckout>,
}

impl ProjectDiscoveryIndex {
    pub fn offers(&self) -> Vec<ProjectCheckoutOffer> {
        self.entries
            .values()
            .map(|entry| entry.offer.clone())
            .collect()
    }

    fn entry(&self, selection_id: &str) -> Option<&DiscoveredCheckout> {
        self.entries.get(selection_id)
    }
}

/// Discover canonical Git roots without following authority outside an
/// approved directory or walking deeper than the owner-supplied bound.
pub fn discover_project_checkouts(
    roots: &[ProjectDiscoveryRoot],
    participant: &str,
) -> Result<ProjectDiscoveryIndex> {
    let mut index = ProjectDiscoveryIndex::default();
    for root in roots {
        if !root.allowed_participants.contains(participant) {
            continue;
        }
        let canonical_root = root
            .path
            .canonicalize()
            .with_context(|| format!("canonicalize discovery root {}", root.label))?;
        let mut pending = VecDeque::from([(canonical_root.clone(), 0usize)]);
        let mut visited = BTreeSet::new();
        while let Some((candidate, depth)) = pending.pop_front() {
            let canonical = match candidate.canonicalize() {
                Ok(path) if path.starts_with(&canonical_root) => path,
                _ => continue,
            };
            if !visited.insert(canonical.clone()) {
                continue;
            }
            if git_root_is_exact(&canonical) {
                let identity = checkout_identity(&canonical)?;
                let digest = identity.strip_prefix("checkout-v1:").unwrap_or(&identity);
                let selection_id = format!("{}:{}", root.label, &digest[..12.min(digest.len())]);
                let checkout_label = canonical
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("checkout")
                    .to_owned();
                index.entries.insert(
                    selection_id.clone(),
                    DiscoveredCheckout {
                        offer: ProjectCheckoutOffer {
                            selection_id,
                            project_label: root.label.clone(),
                            checkout_label,
                        },
                        path: canonical.clone(),
                        identity,
                        approvers: root.project_approvers.clone(),
                    },
                );
            }
            if depth == root.maximum_depth {
                continue;
            }
            let Ok(children) = fs::read_dir(&canonical) else {
                continue;
            };
            for child in children.flatten() {
                if child.file_name() == ".git" {
                    continue;
                }
                if child
                    .file_type()
                    .is_ok_and(|kind| kind.is_dir() || kind.is_symlink())
                {
                    pending.push_back((child.path(), depth + 1));
                }
            }
        }
    }
    Ok(index)
}

fn git_root_is_exact(path: &Path) -> bool {
    crate::git::run_stdout(path, &["rev-parse", "--show-toplevel"], "discover Git root")
        .ok()
        .and_then(|root| PathBuf::from(root).canonicalize().ok())
        .is_some_and(|root| root == path)
}
