use super::validate_issue_worktree;
use crate::integration::git;
use anyhow::{Context, Result, ensure};
use std::{os::unix::ffi::OsStrExt, path::Path};

async fn registered(repo: &Path, path: &Path) -> Result<bool> {
    let listing = git::run(repo, &["worktree", "list", "--porcelain", "-z"]).await?;
    Ok(listing
        .split(|b| *b == 0)
        .any(|field| field.strip_prefix(b"worktree ") == Some(path.as_os_str().as_bytes())))
}

/// Only remove an unchanged, clean, registered worktree. Branches/commits remain
/// available for diagnosis. A lost response is reconciled by observing absence
/// in both Git and the filesystem; partial removals require operator inspection.
pub(crate) async fn remove_completed_worktree(
    repo: &Path,
    path: &Path,
    branch: &str,
    revision: &str,
) -> Result<()> {
    let parent = repo
        .parent()
        .context("Repository has no parent")?
        .join(".worktrees")
        .join(repo.file_name().context("Repository has no name")?);
    ensure!(
        path.parent() == Some(parent.as_path()),
        "Not a managed worktree path"
    );
    for ancestor in path.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(_) => ensure!(
                ancestor.canonicalize()? == ancestor,
                "Worktree path contains symlinks"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let exists = path.try_exists()?;
    let registered = registered(repo, path).await?;
    if !exists && !registered {
        return Ok(());
    }
    ensure!(
        exists && registered,
        "Partial worktree removal or allocation; preserved for inspection"
    );
    validate_issue_worktree(repo, path, branch).await?;
    git::clean(path, revision).await?;
    ensure!(
        git::run(
            path,
            &[
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--ignored",
                "--ignore-submodules=none"
            ]
        )
        .await?
        .is_empty(),
        "Worktree contains untracked or ignored files; preserved for inspection"
    );
    git::run(
        repo,
        &[
            "worktree",
            "remove",
            "--",
            path.to_str().context("Worktree is not UTF-8")?,
        ],
    )
    .await?;
    ensure!(
        !path.try_exists()? && !self::registered(repo, path).await?,
        "Worktree removal could not be confirmed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cleanup_preserves_changed_work_and_reconciles_a_lost_removal_response() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::repositories::RepositoryRoot::open(temp.path()).unwrap();
        let repo = root.create("project").await.unwrap();
        std::fs::write(repo.join("file"), "original").unwrap();
        std::fs::write(repo.join(".gitignore"), "ignored\n").unwrap();
        let sha = git::snapshot(&repo, 1).await.unwrap();
        let path = temp.path().join(".worktrees/project/attempt");
        super::super::create_issue_worktree(&repo, &path, "codex/attempt")
            .await
            .unwrap();
        std::fs::write(path.join("file"), "diagnostic").unwrap();
        assert!(
            remove_completed_worktree(&repo, &path, "codex/attempt", &sha)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(path.join("file")).unwrap(),
            "diagnostic"
        );
        std::fs::write(path.join("file"), "original").unwrap();
        for name in ["untracked", "ignored"] {
            std::fs::write(path.join(name), "keep").unwrap();
            assert!(
                remove_completed_worktree(&repo, &path, "codex/attempt", &sha)
                    .await
                    .is_err()
            );
            std::fs::remove_file(path.join(name)).unwrap();
        }
        assert!(
            remove_completed_worktree(&repo, &path, "other", &sha)
                .await
                .is_err()
        );
        assert!(
            remove_completed_worktree(&repo, &path, "codex/attempt", &"a".repeat(40))
                .await
                .is_err()
        );
        remove_completed_worktree(&repo, &path, "codex/attempt", &sha)
            .await
            .unwrap();
        // Simulate a crash before the database records the successful removal.
        remove_completed_worktree(&repo, &path, "codex/attempt", &sha)
            .await
            .unwrap();
        assert_eq!(
            git::revision(&repo, "refs/heads/codex/attempt")
                .await
                .unwrap(),
            sha
        );
        assert!(repo.join("file").exists());
    }

    #[tokio::test]
    async fn cleanup_rejects_symlinks_and_missing_registered_worktrees() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::repositories::RepositoryRoot::open(temp.path()).unwrap();
        let repo = root.create("project").await.unwrap();
        std::fs::write(repo.join("file"), "original").unwrap();
        let sha = git::snapshot(&repo, 1).await.unwrap();
        let path = temp.path().join(".worktrees/project/attempt");
        super::super::create_issue_worktree(&repo, &path, "codex/attempt")
            .await
            .unwrap();
        let moved = temp.path().join("preserved");
        std::fs::rename(&path, &moved).unwrap();
        assert!(
            remove_completed_worktree(&repo, &path, "codex/attempt", &sha)
                .await
                .is_err()
        );
        std::os::unix::fs::symlink(&moved, &path).unwrap();
        assert!(
            remove_completed_worktree(&repo, &path, "codex/attempt", &sha)
                .await
                .is_err()
        );
        assert!(moved.join("file").exists());
        assert!(
            remove_completed_worktree(&repo, &repo, "main", &sha)
                .await
                .is_err()
        );
    }
}
