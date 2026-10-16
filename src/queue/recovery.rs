use crate::persistence::{self, Store};
use anyhow::Result;
use uuid::Uuid;

impl Store {
    /// Startup only, after the old processes have stopped. Record the interrupted
    /// phase before inspecting external state; never turn a claim back into queued.
    pub async fn reconcile_issues(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE issue_validations SET state='interrupted',error='Service restarted during validation; checks were not replayed',finished_at=now() WHERE state='running'")
            .execute(&mut *tx).await?;
        let runs: Vec<(Uuid, Option<Uuid>, i64, String, String)> = sqlx::query_as(
            "UPDATE issue_runs SET recovery_phase=state,
             state=CASE WHEN id IN (SELECT id FROM integrated_issue_runs) THEN 'releasing' ELSE 'blocked' END,
             validation_id=CASE WHEN EXISTS(SELECT 1 FROM issue_integrations i WHERE i.run_id=issue_runs.id) THEN validation_id ELSE NULL END,
             last_error=CASE WHEN id IN (SELECT id FROM integrated_issue_runs)
                THEN 'Release awaits operator verification of the recorded merge SHA and /complete. No deployment was started or repeated.'
                ELSE 'Service restarted during ' || state || '. Inspect /queue status and the preserved worktrees; dismiss a safely stopped attempt before /queue retry. No work was replayed.' END,
             updated_at=now()
             WHERE state IN ('preparing','implementing','validating','integrating','releasing')
             RETURNING id,session_id,issue_number,recovery_phase,last_error")
            .fetch_all(&mut *tx).await?;
        for (run, session, number, phase, reason) in runs {
            if let Some(session) = session {
                let text = format!("Issue #{number}: {reason}");
                sqlx::query("UPDATE sessions SET status_snapshot=jsonb_set(status_snapshot,'{activity}',to_jsonb($2::text)),updated_at=now() WHERE id=$1")
                    .bind(session).bind(&text).execute(&mut *tx).await?;
                persistence::enqueue(
                    &mut tx,
                    session,
                    &format!("issue-recovery:{run}:{phase}"),
                    &text,
                    None,
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn block_issue_dispatch(&self, run: &super::Run, reason: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let session: Option<Uuid> = sqlx::query_scalar("UPDATE issue_runs SET state='blocked',last_error=$3,updated_at=now() WHERE id=$1 AND claim_id=$2 AND state IN ('preparing','implementing') RETURNING session_id")
            .bind(run.id).bind(run.claim_id).bind(reason).fetch_optional(&mut *tx).await?.flatten();
        if let Some(session) = session {
            persistence::enqueue(
                &mut tx,
                session,
                &format!("issue-recovery:{}:dispatch", run.id),
                reason,
                None,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
