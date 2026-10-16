use crate::persistence::Store;
use anyhow::{Result, ensure};
use uuid::Uuid;

impl Store {
    /// Reserve before forwarding any follow-up that could authorize publication.
    /// The user's actual text defines its scope; a comment is not blanket approval.
    /// Keep the slot through idle turns and restart until explicit completion/dismissal.
    pub(crate) async fn claim_issue_workflow(
        &self,
        session: Uuid,
        source: &str,
        interactive: bool,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let run: super::Run =
            sqlx::query_as("SELECT * FROM issue_runs WHERE session_id=$1 FOR UPDATE")
                .bind(session)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(
            run.workflow == "repository"
                && matches!(run.state.as_str(), "implementing" | "awaiting_input"),
            "This issue is stopped; inspect /queue status before dismissing and retrying"
        );
        let unresolved: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM inbound_requests WHERE session_id=$1 AND discord_id<>$2 AND state IN ('received','sending','uncertain')) OR (NOT $3 AND EXISTS(SELECT 1 FROM pending_requests WHERE session_id=$1 AND state IN ('pending','responding')))")
            .bind(session).bind(source).bind(interactive).fetch_one(&mut *tx).await?;
        ensure!(
            !unresolved,
            "Resolve pending approvals or uncertain input before continuing this issue"
        );
        sqlx::query("INSERT INTO issue_release_locks(repository_id,run_id,github_repository) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(run.repository_id).bind(run.id).bind(&run.github_repository).execute(&mut *tx).await?;
        let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_release_locks WHERE run_id=$1 AND repository_id=$2 AND lower(github_repository)=lower($3))")
            .bind(run.id).bind(run.repository_id).bind(&run.github_repository).fetch_one(&mut *tx).await?;
        ensure!(
            owned,
            "Another issue owns this repository's integration/release slot. Complete or dismiss that task, then send your instructions again; this message was not queued."
        );
        tx.commit().await?;
        Ok(())
    }
}
