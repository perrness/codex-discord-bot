//! Durable dispatch. Claims are never recycled on a timer: an expired external
//! operation may still have executed. Restart requires explicit operator review.
mod integration;
mod recovery;
mod releases;
mod validation;
mod workflow;
use crate::persistence::Store;
use anyhow::{Result, ensure};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Queued,
    Preparing,
    Implementing,
    AwaitingInput,
    Completed,
    Validating,
    Integrating,
    Releasing,
    Released,
    Blocked,
    Failed,
    Cancelled,
}
impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Preparing => "preparing",
            Self::Implementing => "implementing",
            Self::AwaitingInput => "awaiting_input",
            Self::Completed => "completed",
            Self::Validating => "validating",
            Self::Integrating => "integrating",
            Self::Releasing => "releasing",
            Self::Released => "released",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn permits(self, next: Self) -> bool {
        use State::*;
        matches!(
            (self, next),
            (Queued, Preparing)
                | (Preparing, Implementing)
                | (Implementing, AwaitingInput)
                | (AwaitingInput, Implementing)
                | (AwaitingInput, Completed)
                | (Implementing, Validating)
                | (Validating, Integrating)
                | (Integrating, Releasing)
                | (Releasing, Released)
        ) || (matches!(next, Blocked | Failed | Cancelled)
            && !matches!(self, Completed | Released | Failed | Cancelled | Blocked))
    }
}
#[derive(Debug, sqlx::FromRow)]
pub struct Run {
    pub id: Uuid,
    pub repository_id: Uuid,
    pub github_repository: String,
    pub issue_number: i64,
    pub attempt: i32,
    pub state: String,
    pub workflow: String,
    pub claim_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
}
impl Store {
    /// Operator configuration only; decreasing a limit never cancels live work.
    pub async fn configure_issue_workers(&self, limits: crate::config::IssueWorkers) -> Result<()> {
        limits.validate()?;
        sqlx::query(
            "UPDATE issue_worker_limits SET global_limit=$1,repository_limit=$2 WHERE singleton",
        )
        .bind(limits.global as i32)
        .bind(limits.per_repository as i32)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn enqueue_issue(&self, repo: Uuid, github: &str, number: i64) -> Result<()> {
        self.enqueue_issue_with_workflow(
            repo,
            github,
            number,
            crate::config::IssueWorkflow::default(),
        )
        .await
    }

    pub async fn enqueue_issue_with_workflow(
        &self,
        repo: Uuid,
        github: &str,
        number: i64,
        workflow: crate::config::IssueWorkflow,
    ) -> Result<()> {
        ensure!(number > 0, "Invalid issue number");
        // Rediscovery never retries a failed or completed issue.
        sqlx::query("INSERT INTO issue_runs(id,repository_id,github_repository,issue_number,workflow) SELECT $1,$2,$3,$4,$5 WHERE NOT EXISTS(SELECT 1 FROM issue_runs WHERE repository_id=$2 AND github_repository=$3 AND issue_number=$4) ON CONFLICT DO NOTHING")
            .bind(Uuid::new_v4()).bind(repo).bind(github).bind(number).bind(workflow.as_str()).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn claim_issue(&self, repo: Uuid) -> Result<Option<Run>> {
        let mut tx = self.pool.begin().await?;
        // Global admission precedes the queue and repository locks. Counts and
        // the new claim commit together, even across concurrent scheduler ticks.
        let (global, per_repo): (i32, i32) = sqlx::query_as(
            "SELECT global_limit,repository_limit FROM issue_worker_limits WHERE singleton FOR UPDATE"
        ).fetch_one(&mut *tx).await?;
        // Serialize pause/resume and concurrent scheduler ticks on the same row.
        let enabled: Option<bool> = sqlx::query_scalar(
            "SELECT NOT paused FROM issue_queues WHERE repository_id=$1 FOR UPDATE",
        )
        .bind(repo)
        .fetch_optional(&mut *tx)
        .await?;
        if enabled != Some(true) {
            return Ok(None);
        }
        sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
            .bind(repo)
            .execute(&mut *tx)
            .await?;
        let (active, local): (i64, i64) = sqlx::query_as(
            "SELECT count(*),count(*) FILTER (WHERE repository_id=$1) FROM issue_worker_slots",
        )
        .bind(repo)
        .fetch_one(&mut *tx)
        .await?;
        let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sessions WHERE repository_id=$1 AND repository_claimed AND issue_run_id IS NULL)")
            .bind(repo).fetch_one(&mut *tx).await?;
        if busy || active >= i64::from(global) || local >= i64::from(per_repo) {
            return Ok(None);
        }
        let run = sqlx::query_as("UPDATE issue_runs SET state='preparing',claim_id=$2,claimed_at=now(),last_error=NULL,updated_at=now() WHERE id=(SELECT id FROM issue_runs WHERE repository_id=$1 AND state='queued' AND next_dispatch_at<=now() ORDER BY created_at,issue_number,id LIMIT 1 FOR UPDATE) RETURNING *")
            .bind(repo).bind(Uuid::new_v4()).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        Ok(run)
    }

    /// Dependency discovery is read-only. Only a claim that has never allocated
    /// a session or worktree can return to the queue; this cannot replay a task.
    pub async fn defer_issue_dependencies(&self, run: &Run, reason: &str) -> Result<()> {
        let changed = sqlx::query("UPDATE issue_runs SET state='queued',claim_id=NULL,claimed_at=NULL,last_error=$3,next_dispatch_at=now()+interval '60 seconds',updated_at=now() WHERE id=$1 AND claim_id=$2 AND state='preparing' AND session_id IS NULL AND worktree IS NULL AND branch IS NULL")
            .bind(run.id).bind(run.claim_id).bind(reason).execute(&self.pool).await?.rows_affected();
        ensure!(
            changed == 1,
            "Only an undispatched issue may wait for dependencies"
        );
        Ok(())
    }

    pub async fn require_released_dependencies(
        &self,
        dependencies: &[(String, i64)],
    ) -> Result<()> {
        for (repository, number) in dependencies {
            let pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM (SELECT DISTINCT ON (repository_id) state FROM issue_runs WHERE lower(github_repository)=lower($1) AND issue_number=$2 ORDER BY repository_id,attempt DESC) latest WHERE state NOT IN ('released','completed'))")
                .bind(repository).bind(number).fetch_one(&self.pool).await?;
            ensure!(
                !pending,
                "Dependency {repository}#{number} has not completed its workflow or managed release"
            );
        }
        Ok(())
    }
    pub async fn transition_issue(
        &self,
        run: &Run,
        from: State,
        to: State,
        error: Option<&str>,
    ) -> Result<()> {
        ensure!(from.permits(to), "Invalid issue state transition");
        let changed = sqlx::query("UPDATE issue_runs SET state=$4,last_error=$5,updated_at=now() WHERE id=$1 AND claim_id=$2 AND state=$3 AND ($4 NOT IN ('integrating','releasing','released') OR id IN (SELECT id FROM validated_issue_runs)) AND ($4 NOT IN ('releasing','released') OR id IN (SELECT id FROM integrated_issue_runs)) AND ($4 <> 'released' OR id IN (SELECT id FROM confirmed_issue_releases)) AND ($4 NOT IN ('awaiting_input','completed') OR workflow='repository') AND ($4 <> 'completed' OR id IN (SELECT run_id FROM issue_workflow_completions))")
            .bind(run.id).bind(run.claim_id).bind(from.as_str()).bind(to.as_str()).bind(error).execute(&self.pool).await?.rows_affected();
        ensure!(changed == 1, "Stale issue claim or state");
        Ok(())
    }
    pub async fn require_issue_validation(&self, session: Uuid) -> Result<()> {
        let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_runs WHERE session_id=$1 AND (state NOT IN ('integrating','releasing') OR id NOT IN (SELECT id FROM validated_issue_runs)))")
            .bind(session).fetch_one(&self.pool).await?;
        ensure!(
            !blocked,
            "Trusted issue validation has not passed. Inspect /queue status; blocked attempts require dismissal and explicit /queue retry."
        );
        Ok(())
    }
    pub async fn retry_issue(&self, repo: Uuid, number: i64) -> Result<()> {
        self.retry_issue_with_workflow(repo, number, crate::config::IssueWorkflow::default())
            .await
    }

    pub async fn retry_issue_with_workflow(
        &self,
        repo: Uuid,
        number: i64,
        workflow: crate::config::IssueWorkflow,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT repository_id FROM issue_queues WHERE repository_id=$1 FOR UPDATE")
            .bind(repo)
            .execute(&mut *tx)
            .await?;
        let old: Run = sqlx::query_as("SELECT * FROM issue_runs WHERE repository_id=$1 AND issue_number=$2 ORDER BY attempt DESC LIMIT 1 FOR UPDATE")
            .bind(repo).bind(number).fetch_one(&mut *tx).await?;
        let completed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM issue_workflow_completions WHERE run_id=$1)",
        )
        .bind(old.id)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(
            !completed,
            "Completed repository workflows cannot be retried"
        );
        let unsafe_integration: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_integrations WHERE run_id=$1 AND (state <> 'blocked' OR NOT retry_safe OR merge_sha IS NOT NULL))")
            .bind(old.id).fetch_one(&mut *tx).await?;
        ensure!(
            !unsafe_integration,
            "Integration is merged, open or uncertain. Reconcile GitHub first; close any unmerged PR before retrying"
        );
        ensure!(
            matches!(old.state.as_str(), "blocked" | "failed" | "cancelled"),
            "Only blocked, failed or cancelled issues can be retried"
        );
        if let Some(id) = old.session_id {
            let session = self.session(id).await?;
            ensure!(
                !session.repository_claimed
                    && session.current_turn_id.is_none()
                    && session.terminal_task()
                    && !self.unresolved(id).await?,
                "Finish or dismiss the previous session and resolve uncertain input before retrying"
            );
        }
        sqlx::query("UPDATE issue_runs SET state='cancelled',updated_at=now() WHERE id=$1")
            .bind(old.id)
            .execute(&mut *tx)
            .await?;
        let next = Uuid::new_v4();
        sqlx::query("INSERT INTO issue_runs(id,repository_id,github_repository,issue_number,attempt,workflow) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(next).bind(repo).bind(old.github_repository).bind(number).bind(old.attempt+1).bind(workflow.as_str()).execute(&mut *tx).await?;
        if let Some(session) = old.session_id {
            crate::persistence::enqueue(&mut tx, session, &format!("issue-retry:{next}"),
                &format!("Issue #{number}: attempt {} queued as `{next}`. The previous worktrees are preserved. Dispatch follows the queue's pause setting.", old.attempt + 1), None).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::State::*;
    #[test]
    fn transitions_require_validation_and_never_replay_terminal_work() {
        assert!(Queued.permits(Preparing));
        assert!(Preparing.permits(Implementing));
        assert!(Implementing.permits(AwaitingInput));
        assert!(AwaitingInput.permits(Implementing));
        assert!(AwaitingInput.permits(Completed));
        assert!(!Implementing.permits(Completed));
        assert!(Implementing.permits(Validating));
        assert!(Validating.permits(Integrating));
        assert!(Integrating.permits(Releasing));
        assert!(Releasing.permits(Released));
        for state in [
            Queued,
            Preparing,
            Implementing,
            AwaitingInput,
            Validating,
            Integrating,
            Releasing,
        ] {
            assert!(state.permits(Blocked));
            assert!(state.permits(Failed));
            assert!(state.permits(Cancelled));
        }
        assert!(!Implementing.permits(Released));
        for state in [Completed, Released, Blocked, Failed, Cancelled] {
            assert!(!state.permits(Preparing));
        }
    }
}
