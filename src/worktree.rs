use anyhow::{Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

use crate::git;

/// Disable commit signing in a worktree so external coders can commit
/// without hardware key. Fluent's own git calls get this via the
/// wrapper's `-c commit.gpgsign=false`; this sets the persistent
/// repo-level config for processes outside our control.
pub fn disable_commit_signing(worktree_dir: &Path) -> Result<()> {
    git::run(
        worktree_dir,
        &["config", "commit.gpgsign", "false"],
        "disable commit signing",
    )
}

/// Check if a directory is a git repository.
pub fn is_git_repo(dir: &Path) -> bool {
    git::run_raw(dir, &["rev-parse", "--git-dir"]).is_ok_and(|o| o.status.success())
}

/// Return the repository's common git directory as an absolute path.
pub fn git_common_dir(dir: &Path) -> Result<PathBuf> {
    let path = git::run_stdout(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        "resolve common git directory",
    )?;
    Ok(PathBuf::from(path))
}

/// Require a workspace to belong to the project's Git repository.
pub(crate) fn ensure_same_git_repository(project_root: &Path, workspace_path: &Path) -> Result<()> {
    let source_common = fs::canonicalize(git_common_dir(project_root)?)?;
    let workspace_common = fs::canonicalize(git_common_dir(workspace_path)?)?;
    if source_common != workspace_common {
        bail!(
            "Workspace {} belongs to a different git repository",
            workspace_path.display()
        );
    }
    Ok(())
}

/// Require a workspace to remain registered in the project's worktree set.
pub(crate) fn ensure_registered_worktree(project_root: &Path, workspace_path: &Path) -> Result<()> {
    let expected = fs::canonicalize(workspace_path)?;
    let output = git::run_raw(project_root, &["worktree", "list", "--porcelain"])?;
    if !output.status.success() {
        bail!(
            "Failed to list git worktrees: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(path) = line.strip_prefix("worktree ") else {
            continue;
        };
        if fs::canonicalize(path).is_ok_and(|actual| actual == expected) {
            return Ok(());
        }
    }

    bail!(
        "Workspace {} exists but is not a registered git worktree",
        workspace_path.display()
    )
}

/// Require a candidate workspace to remain the project's registered worktree.
pub(crate) fn ensure_candidate_worktree(project_root: &Path, workspace_path: &Path) -> Result<()> {
    ensure_same_git_repository(project_root, workspace_path)?;
    ensure_registered_worktree(project_root, workspace_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_git_project() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let main_dir = tmp.path().join("main");
        fs::create_dir_all(&main_dir).unwrap();

        git::run(&main_dir, &["init", "-b", "main"], "init").unwrap();
        git::run(&main_dir, &["config", "user.email", "test@test"], "config").unwrap();
        git::run(&main_dir, &["config", "user.name", "test"], "config").unwrap();
        fs::write(main_dir.join("README.md"), "test").unwrap();
        git::run(&main_dir, &["add", "."], "stage").unwrap();
        git::run(&main_dir, &["commit", "-m", "init"], "commit").unwrap();

        tmp
    }

    #[test]
    fn test_is_git_repo() {
        let tmp = setup_git_project();
        assert!(is_git_repo(&tmp.path().join("main")));
        assert!(!is_git_repo(tmp.path()));
    }
}
