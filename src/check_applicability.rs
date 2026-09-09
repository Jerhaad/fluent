//! Resolve declared check inputs below execution and recovery consumers.

use crate::{git, work_model};
use anyhow::Result;
use std::{collections::BTreeMap, path::Path};

pub(crate) fn resolve_check_applicability(
    workspace: &Path,
    definition: &work_model::CheckApplicability,
) -> Result<work_model::ResolvedCheckApplicability> {
    definition.validate().map_err(anyhow::Error::msg)?;
    let mut comparison_commits = BTreeMap::new();
    for name in &definition.comparison_refs {
        let commit = git::run_stdout(
            workspace,
            &["rev-parse", "--verify", &format!("{name}^{{commit}}")],
            "resolve declared comparison commit",
        )?;
        comparison_commits.insert(name.clone(), commit);
    }
    Ok(work_model::ResolvedCheckApplicability {
        definition: definition.clone(),
        comparison_commits,
    })
}

/// Evaluate evidence at one instant without changing lifecycle timestamps.
pub(crate) fn evaluation_time() -> chrono::DateTime<chrono::Utc> {
    #[cfg(feature = "test-support")]
    if let Ok(value) = std::env::var("FLUENT_TEST_EVIDENCE_NOW") {
        return chrono::DateTime::parse_from_rfc3339(&value)
            .expect("test evidence time must be RFC3339")
            .with_timezone(&chrono::Utc);
    }
    chrono::Utc::now()
}
