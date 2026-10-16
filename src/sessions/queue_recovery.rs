use super::*;
use crate::{integration::Integration, queue::Run};
use std::path::Path;

#[derive(sqlx::FromRow)]
struct Cleanup {
    run_id: Uuid,
    kind: String,
    path: String,
    branch: String,
    revision: String,
}

impl Manager {
    /// Called after session recovery, before dispatch/input is enabled. Only
    /// observations can advance an interrupted run; no external work is replayed.
    pub async fn recover_issue_queue(&self) -> Result<()> {
        self.store.reconcile_issues().await?;
        self.reconcile_integrations().await?;
        self.store.recover_releases().await?;
        let runs: Vec<Run> = sqlx::query_as("SELECT * FROM issue_runs WHERE state NOT IN ('queued','completed','released','cancelled','failed')")
            .fetch_all(&self.store.pool).await?;
        for run in runs {
            let result = self.inspect_issue_worktrees(&run).await;
            let error = result
                .err()
                .map(|e| format!("Worktree recovery needs inspection: {e:#}"));
            let mut tx = self.store.pool.begin().await?;
            sqlx::query("UPDATE issue_runs SET worktree_error=$2,updated_at=now() WHERE id=$1")
                .bind(run.id)
                .bind(&error)
                .execute(&mut *tx)
                .await?;
            if let (Some(session), Some(error)) = (run.session_id, error) {
                persistence::enqueue(
                    &mut tx,
                    session,
                    &format!("issue-recovery:{}:worktree", run.id),
                    &error,
                    None,
                )
                .await?;
            }
            tx.commit().await?;
        }
        self.cleanup_completed_worktrees().await
    }

    async fn inspect_issue_worktrees(&self, run: &Run) -> Result<()> {
        let repo: Repository =
            sqlx::query_as("SELECT id,name,path,github_repository FROM repositories WHERE id=$1")
                .bind(run.repository_id)
                .fetch_one(&self.store.pool)
                .await?;
        let main = self.root.validate(&repo.name).await?;
        ensure!(main.to_str() == Some(&repo.path), "Repository path changed");
        let (path, branch): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT worktree,branch FROM issue_runs WHERE id=$1")
                .bind(run.id)
                .fetch_one(&self.store.pool)
                .await?;
        let Some(path) = path else {
            ensure!(
                branch.is_none(),
                "Worktree allocation identity is incomplete"
            );
            if let Some(session) = run.session_id {
                ensure!(
                    self.store.session(session).await?.codex_thread_id.is_none(),
                    "Codex conversation has no worktree allocation"
                );
            }
            return Ok(());
        };
        let expected = self
            .config
            .repos_root
            .join(".worktrees")
            .join(&repo.name)
            .join(run.id.to_string());
        ensure!(
            Path::new(&path) == expected,
            "Issue worktree is outside its managed allocation"
        );
        let expected_branch = format!(
            "codex/issue-{}-attempt-{}-{}",
            run.issue_number, run.attempt, run.id
        );
        ensure!(
            branch.as_deref() == Some(expected_branch.as_str()),
            "Issue branch allocation changed"
        );
        crate::repositories::validate_issue_worktree(&main, Path::new(&path), &expected_branch)
            .await?;
        if let Some(session) = run.session_id {
            ensure!(
                self.store.session(session).await?.working_directory == path,
                "Session worktree differs from issue allocation"
            );
        }
        if let Some(i) = self.store.integration(run.id).await? {
            if let Some(source) = &i.source_sha {
                // Read-only revision/status assertion; preserve changed files.
                crate::integration::git::clean(Path::new(&path), source).await?;
            }
            self.check_integration_worktree(&repo, &i).await?;
        }
        Ok(())
    }

    async fn check_integration_worktree(&self, repo: &Repository, i: &Integration) -> Result<()> {
        let expected = self
            .config
            .repos_root
            .join(".worktrees")
            .join(&repo.name)
            .join(format!("integration-{}", i.run_id));
        ensure!(
            Path::new(&i.worktree) == expected
                && i.branch == format!("codex/integration-{}", i.run_id),
            "Integration worktree allocation changed"
        );
        // Intent can precede Git allocation. Once a candidate was persisted,
        // absence is a recovery error, not permission to recreate the worktree.
        if expected.try_exists()? || i.candidate_sha.is_some() {
            crate::repositories::validate_issue_worktree(
                Path::new(&repo.path),
                &expected,
                &i.branch,
            )
            .await?;
            if let Some(candidate) = &i.candidate_sha {
                // Read-only revision/status assertion; preserve changed files.
                crate::integration::git::clean(&expected, candidate).await?;
            }
        }
        Ok(())
    }

    /// Capture the completion revision under the session gate and a shared
    /// repository lock. Dirty files block removal, not task completion.
    pub(super) async fn workflow_cleanup_revision(
        &self,
        run: &Run,
        session: &Session,
    ) -> Result<String> {
        let repo: Repository =
            sqlx::query_as("SELECT id,name,path,github_repository FROM repositories WHERE id=$1")
                .bind(run.repository_id)
                .fetch_one(&self.store.pool)
                .await?;
        self.take_repository_lock(&repo, session.id, true)?;
        let result = async {
            self.inspect_issue_worktrees(run).await?;
            crate::integration::git::revision(
                Path::new(&session.working_directory),
                "HEAD^{commit}",
            )
            .await
        }
        .await;
        self.drop_repository_lock(session);
        result.context("Could not record the completed worktree revision")
    }

    /// Safe to retry: each attempt re-observes identity and cleanliness before
    /// removing a worktree and its unchanged local branch. Never force removal.
    pub async fn cleanup_completed_worktrees(&self) -> Result<()> {
        self.cleanup_worktrees(None).await
    }

    pub(super) async fn cleanup_worktrees(&self, only_session: Option<Uuid>) -> Result<()> {
        let pending: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT DISTINCT r.id,r.session_id FROM issue_runs r JOIN issue_worktree_cleanup c ON c.run_id=r.id JOIN sessions s ON s.id=r.session_id WHERE ((r.workflow='managed' AND r.state='released' AND r.id IN (SELECT id FROM confirmed_issue_releases)) OR (r.workflow='repository' AND r.state='completed' AND EXISTS(SELECT 1 FROM issue_workflow_completions w WHERE w.run_id=r.id AND w.revision=c.revision))) AND c.state <> 'removed' AND s.channel_state IN ('completed','deleted') AND NOT s.repository_claimed AND s.current_turn_id IS NULL AND ($1::uuid IS NULL OR s.id=$1)")
            .bind(only_session).fetch_all(&self.store.pool).await?;
        for (run, session) in pending {
            let gate = self.gate(session);
            let Ok(_guard) = gate.try_lock() else {
                continue;
            };
            let session = self.store.session(session).await?;
            let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE repository_id=$1 AND repository_claimed)")
                .bind(session.repository_id).fetch_one(&self.store.pool).await?;
            if busy
                || session.repository_claimed
                || session.current_turn_id.is_some()
                || !matches!(session.channel_state.as_str(), "completed" | "deleted")
                || self.store.unresolved(session.id).await?
            {
                continue;
            }
            let repo: Repository = sqlx::query_as(
                "SELECT id,name,path,github_repository FROM repositories WHERE id=$1",
            )
            .bind(session.repository_id)
            .fetch_one(&self.store.pool)
            .await?;
            // The session is terminal and unclaimed under its gate. Discard any
            // retained shared descriptor before requesting exclusive cleanup.
            self.drop_repository_lock(&session);
            if self.take_repository_lock(&repo, session.id, false).is_err() {
                continue;
            }
            let result = self.cleanup_issue_worktrees(&repo, run).await;
            self.drop_repository_lock(&session);
            result?;
        }
        Ok(())
    }

    async fn cleanup_issue_worktrees(&self, repo: &Repository, run: Uuid) -> Result<()> {
        let pending: Vec<Cleanup> = sqlx::query_as("SELECT * FROM issue_worktree_cleanup WHERE run_id=$1 AND state <> 'removed' ORDER BY kind")
            .bind(run).fetch_all(&self.store.pool).await?;
        for cleanup in pending {
            sqlx::query("UPDATE issue_worktree_cleanup SET state='removing',updated_at=now() WHERE run_id=$1 AND kind=$2")
                .bind(run).bind(&cleanup.kind).execute(&self.store.pool).await?;
            let result = self.remove_issue_worktree(repo, &cleanup).await;
            let error = result
                .err()
                .map(|e| format!("{} worktree/branch cleanup blocked: {e:#}", cleanup.kind));
            let mut tx = self.store.pool.begin().await?;
            sqlx::query("UPDATE issue_worktree_cleanup SET state=$3,last_error=$4,updated_at=now() WHERE run_id=$1 AND kind=$2")
                .bind(run).bind(&cleanup.kind).bind(if error.is_some() { "blocked" } else { "removed" }).bind(&error).execute(&mut *tx).await?;
            sqlx::query("UPDATE issue_runs SET worktree_error=NULL WHERE id=$1 AND NOT EXISTS(SELECT 1 FROM issue_worktree_cleanup WHERE run_id=$1 AND state <> 'removed')")
                .bind(run).execute(&mut *tx).await?;
            if let Some(error) = error {
                let session: Uuid =
                    sqlx::query_scalar("SELECT session_id FROM issue_runs WHERE id=$1")
                        .bind(run)
                        .fetch_one(&mut *tx)
                        .await?;
                persistence::enqueue(
                    &mut tx,
                    session,
                    &format!("issue-cleanup:{run}:{}:blocked", cleanup.kind),
                    &error,
                    None,
                )
                .await?;
            }
            tx.commit().await?;
        }
        Ok(())
    }

    async fn remove_issue_worktree(&self, repo: &Repository, c: &Cleanup) -> Result<()> {
        let main = self.root.validate(&repo.name).await?;
        ensure!(main.to_str() == Some(&repo.path), "Repository path changed");
        let run: Run = sqlx::query_as("SELECT * FROM issue_runs WHERE id=$1")
            .bind(c.run_id)
            .fetch_one(&self.store.pool)
            .await?;
        let i = self.store.integration(c.run_id).await?;
        let (path, branch, revision) = if c.kind == "integration" {
            ensure!(run.workflow == "managed", "Unexpected integration cleanup");
            let i = i.context("Missing integration evidence")?;
            (
                format!("integration-{}", c.run_id),
                format!("codex/integration-{}", c.run_id),
                i.candidate_sha,
            )
        } else {
            let revision = if run.workflow == "repository" {
                sqlx::query_scalar(
                    "SELECT revision FROM issue_workflow_completions WHERE run_id=$1",
                )
                .bind(run.id)
                .fetch_one(&self.store.pool)
                .await?
            } else {
                i.context("Missing integration evidence")?.source_sha
            };
            (
                run.id.to_string(),
                format!(
                    "codex/issue-{}-attempt-{}-{}",
                    run.issue_number, run.attempt, run.id
                ),
                revision,
            )
        };
        let expected = self
            .config
            .repos_root
            .join(".worktrees")
            .join(&repo.name)
            .join(path);
        ensure!(
            Path::new(&c.path) == expected
                && c.branch == branch
                && Some(&c.revision) == revision.as_ref(),
            "Cleanup identity differs from the completed attempt"
        );
        crate::repositories::remove_completed_worktree(&main, &expected, &c.branch, &c.revision)
            .await
    }
}
