use super::Policy;
use anyhow::{Context, Result, ensure};
use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::Path};
use tokio::process::Command;

pub(crate) fn command(path: &Path) -> Command {
    let mut command = crate::repositories::git(path);
    command.args([
        "-c",
        "user.name=Codex Integration",
        "-c",
        "user.email=codex-integration@localhost",
        "-c",
        "commit.gpgSign=false",
        "-c",
        "merge.gpgSign=false",
    ]);
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

pub(crate) async fn run(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = command(path);
    command.args(args);
    super::github::output(command, None).await
}

pub(crate) async fn revision(path: &Path, reference: &str) -> Result<String> {
    let output = run(path, &["rev-parse", "--verify", reference]).await?;
    let sha = String::from_utf8(output)?.trim().to_owned();
    ensure!(
        sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid Git SHA"
    );
    Ok(sha)
}

pub(crate) async fn fetch(path: &Path, remote: &str, reference: &str) -> Result<String> {
    ensure!(
        reference.starts_with("refs/"),
        "Expected a full Git reference"
    );
    network(
        path,
        &[
            "fetch",
            "--no-tags",
            "--no-recurse-submodules",
            "--",
            remote,
            reference,
        ],
    )
    .await?;
    revision(path, "FETCH_HEAD^{commit}").await
}

pub(crate) async fn snapshot(path: &Path, issue: i64) -> Result<String> {
    run(path, &["add", "--all", "--", "."]).await?;
    if !run(path, &["diff", "--cached", "--name-only"])
        .await?
        .is_empty()
    {
        run(
            path,
            &["commit", "-m", &format!("Implement issue #{issue}")],
        )
        .await?;
    }
    let sha = revision(path, "HEAD^{commit}").await?;
    clean(path, &sha).await?;
    Ok(sha)
}

pub(crate) async fn candidate(
    repo: &Path,
    worktree: &Path,
    branch: &str,
    base: &str,
    source: &str,
) -> Result<(String, String)> {
    ensure!(
        !worktree.try_exists()?,
        "Integration worktree already exists; inspect the previous attempt"
    );
    // The issue worktree already established this canonical managed parent.
    let parent = worktree
        .parent()
        .context("Integration worktree parent missing")?;
    ensure!(
        parent.canonicalize()? == parent,
        "Integration parent changed"
    );
    let mut add = command(repo);
    add.args(["worktree", "add", "-b", branch])
        .arg(worktree)
        .arg(base);
    super::github::output(add, None).await?;
    run(worktree, &["merge", "--no-ff", "--no-edit", source])
        .await
        .context("Integration merge conflict or merge failure; worktree preserved")?;
    let sha = revision(worktree, "HEAD^{commit}").await?;
    ensure!(
        sha != base,
        "Issue changes are already included in the default branch"
    );
    let tree = revision(worktree, "HEAD^{tree}").await?;
    clean(worktree, &sha).await?;
    Ok((sha, tree))
}

pub(crate) async fn clean(path: &Path, expected: &str) -> Result<()> {
    ensure!(
        revision(path, "HEAD^{commit}").await? == expected,
        "Candidate branch advanced; validation is stale"
    );
    ensure!(
        run(
            path,
            &[
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--ignore-submodules=none"
            ]
        )
        .await?
        .is_empty(),
        "Candidate worktree changed; validation is stale"
    );
    Ok(())
}

pub(crate) async fn release_gate(
    path: &Path,
    base: &str,
    candidate: &str,
    policy: &Policy,
) -> Result<()> {
    let changed = run(
        path,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            base,
            candidate,
            "--",
        ],
    )
    .await?;
    for name in changed.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = Path::new(OsStr::from_bytes(name));
        ensure!(
            !policy.protects(path),
            "Candidate changes release/automation configuration {}; operator review required",
            path.display()
        );
    }
    Ok(())
}

pub(crate) async fn push(path: &Path, remote: &str, sha: &str, branch: &str) -> Result<()> {
    // No force, lease override, implicit refspec, tags, or default-branch push.
    ensure!(
        branch.starts_with("codex/integration-"),
        "Invalid integration branch"
    );
    network(
        path,
        &[
            "push",
            "--porcelain",
            "--",
            remote,
            &format!("{sha}:refs/heads/{branch}"),
        ],
    )
    .await?;
    Ok(())
}

async fn network(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = command(path);
    super::github::auth_environment(&mut command);
    command.args([
        "-c",
        "credential.helper=",
        "-c",
        "credential.helper=!gh auth git-credential",
    ]);
    command.args(args);
    super::github::output(command, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repository() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::repositories::RepositoryRoot::open(temp.path()).unwrap();
        let repo = root.create("project").await.unwrap();
        std::fs::write(repo.join("file"), "initial\n").unwrap();
        let base = snapshot(&repo, 1).await.unwrap();
        (temp, repo, base)
    }

    #[tokio::test]
    async fn merges_latest_base_without_changing_issue_branch_and_detects_stale_validation() {
        let (temp, repo, base) = repository().await;
        let issue = temp.path().join("issue");
        crate::repositories::create_issue_worktree(&repo, &issue, "issue")
            .await
            .unwrap();
        std::fs::write(issue.join("implementation"), "new\n").unwrap();
        let source = snapshot(&issue, 1).await.unwrap();
        std::fs::write(repo.join("upstream"), "latest\n").unwrap();
        let latest = snapshot(&repo, 2).await.unwrap();
        assert_ne!(latest, base);
        let path = temp.path().join("candidate");
        let (head, tree) = candidate(&repo, &path, "codex/integration-test", &latest, &source)
            .await
            .unwrap();
        assert!(path.join("implementation").exists() && path.join("upstream").exists());
        assert_eq!(revision(&issue, "HEAD").await.unwrap(), source);
        assert_eq!(revision(&path, "HEAD^{tree}").await.unwrap(), tree);
        clean(&path, &head).await.unwrap();
        std::fs::write(path.join("file"), "changed after checks\n").unwrap();
        assert!(clean(&path, &head).await.is_err());
        snapshot(&path, 3).await.unwrap();
        assert!(clean(&path, &head).await.is_err());
        assert!(
            candidate(&repo, &path, "codex/integration-test", &latest, &source)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn conflicts_preserve_both_branches_and_worktree_for_diagnosis() {
        let (temp, repo, _) = repository().await;
        let issue = temp.path().join("issue");
        crate::repositories::create_issue_worktree(&repo, &issue, "issue")
            .await
            .unwrap();
        std::fs::write(issue.join("file"), "issue\n").unwrap();
        let source = snapshot(&issue, 1).await.unwrap();
        std::fs::write(repo.join("file"), "upstream\n").unwrap();
        let base = snapshot(&repo, 2).await.unwrap();
        let path = temp.path().join("candidate");
        assert!(
            candidate(&repo, &path, "codex/integration-test", &base, &source)
                .await
                .unwrap_err()
                .to_string()
                .contains("conflict")
        );
        assert!(path.exists());
        assert_eq!(revision(&repo, "HEAD").await.unwrap(), base);
        assert_eq!(revision(&issue, "HEAD").await.unwrap(), source);
    }

    #[tokio::test]
    async fn non_fast_forward_push_is_rejected_and_workflow_changes_block_release_gate() {
        let (temp, repo, first) = repository().await;
        let remote = temp.path().join("remote.git");
        run(temp.path(), &["init", "--bare", remote.to_str().unwrap()])
            .await
            .unwrap();
        push(
            &repo,
            remote.to_str().unwrap(),
            &first,
            "codex/integration-test",
        )
        .await
        .unwrap();
        std::fs::create_dir_all(repo.join(".github/workflows")).unwrap();
        std::fs::write(repo.join(".github/workflows/deploy.yml"), "on: push\n").unwrap();
        let second = snapshot(&repo, 2).await.unwrap();
        push(
            &repo,
            remote.to_str().unwrap(),
            &second,
            "codex/integration-test",
        )
        .await
        .unwrap();
        assert!(
            push(
                &repo,
                remote.to_str().unwrap(),
                &first,
                "codex/integration-test"
            )
            .await
            .is_err()
        );
        let policy = Policy {
            github_repository: "a/b".into(),
            release_gate: super::super::ReleaseGate::DeploymentsDisabled,
            protected_paths: vec![],
            checks_timeout_seconds: 10,
        };
        assert!(release_gate(&repo, &first, &second, &policy).await.is_err());
    }
}
