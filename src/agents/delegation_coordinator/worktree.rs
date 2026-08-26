pub(crate) fn worktree_branch_name(agent_name: &str, worktree_id: &str) -> String {
    format!("subagent/{}_{}", agent_name, worktree_id)
}

use super::DelegationCoordinator;

impl DelegationCoordinator {
    /// Remove orphaned worktree directories left behind by crashed or
    /// timed-out sub-agent runs. Called once at daemon startup.
    ///
    /// Runs `git -C <dir> worktree remove` from the leftover directory
    /// itself: git locates the owning repository through the worktree's
    /// `.git` file (the startup path has no per-agent workspace context, and
    /// `worktrees_root` itself is not a repository). `worktree remove` also
    /// clears the worktree metadata in the owning repo, so no separate
    /// `prune` is needed on the success path.
    pub fn cleanup_stale_worktrees(&self) {
        let entries = match std::fs::read_dir(&self.worktrees_root) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut cleaned = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            // Each worktree dir is named like `coder_<8hex>`.
            // `git -C <dir> worktree remove --force` also removes stale git worktree metadata.
            let out = std::process::Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(&path)
                .current_dir(&path)
                .output();
            let ok = out.as_ref().is_ok_and(|o| o.status.success());
            if !ok {
                // Fallback: remove directory directly if git doesn't know about it.
                let _ = std::fs::remove_dir_all(&path);
            }
            cleaned += 1;
        }
        tracing::debug!(cleaned, root = %self.worktrees_root.display(), "cleaned stale worktrees");
        if cleaned > 0 {
            tracing::info!(count = cleaned, "cleaned up stale sub-agent worktrees");
        }
    }
}
