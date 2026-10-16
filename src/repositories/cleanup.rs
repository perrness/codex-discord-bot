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

/// Only remove an unchanged, clean, registered worktree and its recorded local
/// branch. Lost responses are reconciled by observing the filesystem, Git
/// registration and branch; partial worktree removals require inspection.
pub(crate) async fn remove_completed_worktree(
    repo: &Path,
    path: &Path,
    branch: &str,
    revision: &str,
) -> Result<()> {
    ensure!(branch.starts_with("codex/"), "Not a managed branch");
    git::run(repo, &["check-ref-format", &format!("refs/heads/{branch}")]).await?;
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
        return remove_completed_branch(repo, branch, revision).await;
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
    remove_completed_branch(repo, branch, revision).await
}

/// Check the exact ref, including symbolic refs and changes made after worktree
/// removal. A missing branch is success when reconciling an interrupted cleanup.
async fn branch_present(repo: &Path, reference: &str, revision: &str) -> Result<bool> {
    let listing = git::run(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
            reference,
        ],
    )
    .await?;
    let listing = String::from_utf8(listing)?;
    for line in listing.lines() {
        let mut fields = line.split('\0');
        if fields.next() == Some(reference) {
            ensure!(
                fields.next() == Some(revision) && fields.next() == Some(""),
                "Local branch changed since completion; preserved for inspection"
            );
            return Ok(true);
        }
    }
    Ok(false)
}

async fn remove_completed_branch(repo: &Path, branch: &str, revision: &str) -> Result<()> {
    let reference = format!("refs/heads/{branch}");
    let listing = git::run(repo, &["worktree", "list", "--porcelain", "-z"]).await?;
    ensure!(
        !listing
            .split(|b| *b == 0)
            .any(|field| field.strip_prefix(b"branch ") == Some(reference.as_bytes())),
        "Local branch is still checked out; preserved for inspection"
    );
    if branch_present(repo, &reference, revision).await? {
        // Compare-and-delete only the confirmed ref, even after squash merges or
        // when the registered checkout has not fetched the merged default branch.
        git::run(
            repo,
            &["update-ref", "--no-deref", "-d", &reference, revision],
        )
        .await?;
        ensure!(
            !branch_present(repo, &reference, revision).await?,
            "Local branch removal could not be confirmed"
        );
    }
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
        assert!(
            git::revision(&repo, "refs/heads/codex/attempt")
                .await
                .is_err()
        );
        assert!(repo.join("file").exists());
    }

    #[tokio::test]
    async fn cleanup_preserves_changed_symbolic_and_checked_out_branches_after_worktree_removal() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::repositories::RepositoryRoot::open(temp.path()).unwrap();
        let repo = root.create("project").await.unwrap();
        std::fs::write(repo.join("file"), "original").unwrap();
        let sha = git::snapshot(&repo, 1).await.unwrap();
        let path = temp.path().join(".worktrees/project/attempt");
        super::super::create_issue_worktree(&repo, &path, "codex/attempt")
            .await
            .unwrap();
        git::run(&repo, &["worktree", "remove", path.to_str().unwrap()])
            .await
            .unwrap();
        // Crash between worktree and branch removal, followed by an external edit.
        std::fs::write(repo.join("file"), "new work").unwrap();
        let newer = git::snapshot(&repo, 2).await.unwrap();
        git::run(&repo, &["update-ref", "refs/heads/codex/attempt", &newer])
            .await
            .unwrap();
        assert!(
            remove_completed_worktree(&repo, &path, "codex/attempt", &sha)
                .await
                .is_err()
        );
        assert_eq!(
            git::revision(&repo, "refs/heads/codex/attempt")
                .await
                .unwrap(),
            newer
        );
        git::run(&repo, &["update-ref", "refs/heads/codex/attempt", &sha])
            .await
            .unwrap();
        let other = temp.path().join("other");
        git::run(
            &repo,
            &["worktree", "add", other.to_str().unwrap(), "codex/attempt"],
        )
        .await
        .unwrap();
        assert!(
            remove_completed_worktree(&repo, &path, "codex/attempt", &sha)
                .await
                .is_err()
        );
        assert!(other.join("file").exists());
        git::run(&repo, &["worktree", "remove", other.to_str().unwrap()])
            .await
            .unwrap();
        git::run(&repo, &["branch", "saved", &sha]).await.unwrap();
        git::run(
            &repo,
            &[
                "symbolic-ref",
                "refs/heads/codex/attempt",
                "refs/heads/saved",
            ],
        )
        .await
        .unwrap();
        assert!(
            remove_completed_worktree(&repo, &path, "codex/attempt", &sha)
                .await
                .is_err()
        );
        assert_eq!(git::revision(&repo, "refs/heads/saved").await.unwrap(), sha);
        git::run(
            &repo,
            &["symbolic-ref", "--delete", "refs/heads/codex/attempt"],
        )
        .await
        .unwrap();
        git::run(&repo, &["update-ref", "refs/heads/codex/attempt", &sha])
            .await
            .unwrap();
        // Recovery completes branch cleanup without recreating the worktree.
        remove_completed_worktree(&repo, &path, "codex/attempt", &sha)
            .await
            .unwrap();
        remove_completed_worktree(&repo, &path, "codex/attempt", &sha)
            .await
            .unwrap();
        assert!(
            git::revision(&repo, "refs/heads/codex/attempt")
                .await
                .is_err()
        );
        assert_eq!(
            git::revision(&repo, "refs/heads/main").await.unwrap(),
            newer
        );
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
