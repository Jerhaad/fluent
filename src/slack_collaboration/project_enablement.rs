use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::checkout_identity;
use crate::setup::{ConfiguredSetup, InitSetupInputs};

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

/// Complete configured-init choices collected before checkout mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackInitChoices {
    pub coder_profile: String,
    pub follow_up_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_coder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_coder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_effort: Option<String>,
}

impl SlackInitChoices {
    fn configured_setup(&self) -> Result<ConfiguredSetup> {
        ConfiguredSetup::from_inputs(InitSetupInputs {
            coder_profile: Some(self.coder_profile.clone()),
            follow_up_mode: Some(self.follow_up_mode.clone()),
            write_coder: self.write_coder.clone(),
            write_model: self.write_model.clone(),
            write_effort: self.write_effort.clone(),
            review_coder: self.review_coder.clone(),
            review_model: self.review_model.clone(),
            review_effort: self.review_effort.clone(),
        })?
        .context("Slack project setup requires configured initialization choices")
    }
}

/// Exact path-free setup proposal presented for Slack approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackSetupProposal {
    pub selection_id: String,
    pub project_label: String,
    pub checkout_label: String,
    pub choices: SlackInitChoices,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredInitReport {
    pub already_initialized: bool,
    pub summary: String,
}

/// Existing configured-initialization boundary used by Slack setup. Its
/// implementation owns repository, durable-path, host, and provider preflights.
pub trait ConfiguredInitBoundary {
    fn initialize(
        &mut self,
        checkout: &Path,
        choices: &SlackInitChoices,
    ) -> Result<ConfiguredInitReport>;
}

pub fn prepare_slack_project_setup(
    index: &ProjectDiscoveryIndex,
    selection_id: &str,
    choices: SlackInitChoices,
) -> Result<SlackSetupProposal> {
    choices.configured_setup()?;
    let entry = index
        .entry(selection_id)
        .context("selected checkout is unavailable")?;
    let mut proposal = SlackSetupProposal {
        selection_id: selection_id.to_owned(),
        project_label: entry.offer.project_label.clone(),
        checkout_label: entry.offer.checkout_label.clone(),
        choices,
        revision: String::new(),
    };
    proposal.revision = setup_revision(&proposal)?;
    Ok(proposal)
}

pub fn approve_slack_project_setup(
    current_index: &ProjectDiscoveryIndex,
    proposal: &SlackSetupProposal,
    reviewed_revision: &str,
    actor: &str,
    boundary: &mut dyn ConfiguredInitBoundary,
) -> Result<ConfiguredInitReport> {
    if reviewed_revision != proposal.revision || setup_revision(proposal)? != proposal.revision {
        anyhow::bail!("project setup approval is stale; review the current proposal")
    }
    proposal.choices.configured_setup()?;
    let entry = current_index
        .entry(&proposal.selection_id)
        .context("selected checkout is no longer available")?;
    if entry.offer.project_label != proposal.project_label
        || entry.offer.checkout_label != proposal.checkout_label
        || !entry.approvers.contains(actor)
    {
        anyhow::bail!("current project authority does not permit setup")
    }
    if checkout_identity(&entry.path)? != entry.identity {
        anyhow::bail!("selected checkout identity changed; select it again")
    }
    boundary.initialize(&entry.path, &proposal.choices)
}

fn setup_revision(proposal: &SlackSetupProposal) -> Result<String> {
    let mut reviewed = proposal.clone();
    reviewed.revision.clear();
    Ok(format!(
        "setup-v1:{:x}",
        Sha256::digest(serde_json::to_vec(&reviewed)?)
    ))
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
