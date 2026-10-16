//! Durable dispatch. Claims are never recycled on a timer: an expired external
//! operation may still have executed. Restart requires explicit operator review.
mod integration;
mod recovery;
mod validation;
use crate::persistence::Store;
use anyhow::{Result, ensure};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Queued,
    Preparing,
    Implementing,
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
                | (Implementing, Validating)
                | (Validating, Integrating)
                | (Integrating, Releasing)
                | (Releasing, Released)
        ) || (matches!(next, Blocked | Failed | Cancelled)
            && !matches!(self, Released | Failed | Cancelled | Blocked))
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
    pub claim_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
}
impl Store {
    pub async fn enqueue_issue(&self, repo: Uuid, github: &str, number: i64) -> Result<()> {
        ensure!(number > 0, "Invalid issue number");
        // Rediscovery never retries a failed or completed issue.
        sqlx::query("INSERT INTO issue_runs(id,repository_id,github_repository,issue_number) SELECT $1,$2,$3,$4 WHERE NOT EXISTS(SELECT 1 FROM issue_runs WHERE repository_id=$2 AND github_repository=$3 AND issue_number=$4) ON CONFLICT DO NOTHING")
            .bind(Uuid::new_v4()).bind(repo).bind(github).bind(number).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn claim_issue(&self, repo: Uuid) -> Result<Option<Run>> {
        let mut tx = self.pool.begin().await?;
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
        let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_runs WHERE repository_id=$1 AND state IN ('preparing','implementing','validating','integrating','releasing','blocked')) OR EXISTS(SELECT 1 FROM sessions WHERE repository_id=$1 AND repository_claimed) OR EXISTS(SELECT 1 FROM issue_integrations i JOIN issue_runs r ON r.id=i.run_id WHERE i.repository_id=$1 AND ((i.state <> 'merged' AND NOT i.retry_safe) OR (i.merge_sha IS NOT NULL AND r.state <> 'released')))")
            .bind(repo).fetch_one(&mut *tx).await?;
        if busy {
            return Ok(None);
        }
        let run = sqlx::query_as("UPDATE issue_runs SET state='preparing',claim_id=$2,claimed_at=now(),updated_at=now() WHERE id=(SELECT id FROM issue_runs WHERE repository_id=$1 AND state='queued' ORDER BY created_at,issue_number,id LIMIT 1 FOR UPDATE) RETURNING *")
            .bind(repo).bind(Uuid::new_v4()).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        Ok(run)
    }
    pub async fn transition_issue(
        &self,
        run: &Run,
        from: State,
        to: State,
        error: Option<&str>,
    ) -> Result<()> {
        ensure!(from.permits(to), "Invalid issue state transition");
        let changed = sqlx::query("UPDATE issue_runs SET state=$4,last_error=$5,updated_at=now() WHERE id=$1 AND claim_id=$2 AND state=$3 AND ($4 NOT IN ('integrating','releasing','released') OR id IN (SELECT id FROM validated_issue_runs)) AND ($4 NOT IN ('releasing','released') OR id IN (SELECT id FROM integrated_issue_runs)) AND ($4 <> 'released' OR id IN (SELECT id FROM confirmed_issue_releases))")
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
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT repository_id FROM issue_queues WHERE repository_id=$1 FOR UPDATE")
            .bind(repo)
            .execute(&mut *tx)
            .await?;
        let old: Run = sqlx::query_as("SELECT * FROM issue_runs WHERE repository_id=$1 AND issue_number=$2 ORDER BY attempt DESC LIMIT 1 FOR UPDATE")
            .bind(repo).bind(number).fetch_one(&mut *tx).await?;
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
        sqlx::query("INSERT INTO issue_runs(id,repository_id,github_repository,issue_number,attempt) VALUES($1,$2,$3,$4,$5)")
            .bind(next).bind(repo).bind(old.github_repository).bind(number).bind(old.attempt+1).execute(&mut *tx).await?;
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
        assert!(Implementing.permits(Validating));
        assert!(Validating.permits(Integrating));
        assert!(Integrating.permits(Releasing));
        assert!(Releasing.permits(Released));
        for state in [
            Queued,
            Preparing,
            Implementing,
            Validating,
            Integrating,
            Releasing,
        ] {
            assert!(state.permits(Blocked));
            assert!(state.permits(Failed));
            assert!(state.permits(Cancelled));
        }
        assert!(!Implementing.permits(Released));
        for state in [Released, Blocked, Failed, Cancelled] {
            assert!(!state.permits(Preparing));
        }
    }
}
