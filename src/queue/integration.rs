use crate::{
    integration::{Integration, Policy},
    persistence::{self, Store},
    queue::Run,
    validation::{self, CheckResult, Decision},
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use uuid::Uuid;

impl Store {
    pub async fn integration(&self, run: Uuid) -> Result<Option<Integration>> {
        Ok(
            sqlx::query_as("SELECT * FROM issue_integrations WHERE run_id=$1")
                .bind(run)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn begin_issue_integration(
        &self,
        run: &Run,
        policy: &Policy,
        worktree: &str,
    ) -> Result<Integration> {
        self.try_begin_issue_integration(run, policy, worktree)
            .await?
            .context("Repository integration/release slot is occupied")
    }

    /// Contention is a wait, not a failed attempt. The durable slot is retained
    /// through release confirmation, and through any uncertain remote mutation.
    pub async fn try_begin_issue_integration(
        &self,
        run: &Run,
        policy: &Policy,
        worktree: &str,
    ) -> Result<Option<Integration>> {
        policy.validate()?;
        ensure!(
            policy.github_repository == run.github_repository,
            "Trusted integration repository differs from issue mapping"
        );
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT repository_id FROM issue_queues WHERE repository_id=$1 FOR UPDATE")
            .bind(run.repository_id)
            .execute(&mut *tx)
            .await?;
        let attempted: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_integrations WHERE run_id=$1)")
                .bind(run.id)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(
            !attempted,
            "Integration already attempted; reconcile instead of replaying"
        );
        let next: Option<Uuid> = sqlx::query_scalar("SELECT id FROM issue_runs WHERE repository_id=$1 AND state='integrating' AND NOT EXISTS(SELECT 1 FROM issue_integrations WHERE run_id=issue_runs.id) ORDER BY integration_ready_at NULLS LAST,created_at,id LIMIT 1")
            .bind(run.repository_id).fetch_optional(&mut *tx).await?;
        if next != Some(run.id) {
            return Ok(None);
        }
        let acquired = sqlx::query("INSERT INTO issue_release_locks(repository_id,run_id,github_repository) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(run.repository_id).bind(run.id).bind(&run.github_repository).execute(&mut *tx).await?.rows_affected();
        if acquired == 0 {
            return Ok(None);
        }
        let integration = sqlx::query_as("INSERT INTO issue_integrations(run_id,repository_id,validation_id,policy,validation_policy,branch,worktree) SELECT r.id,r.repository_id,r.validation_id,$3,v.policy,$4,$5 FROM issue_runs r JOIN issue_validations v ON v.id=r.validation_id JOIN sessions s ON s.id=r.session_id WHERE r.id=$1 AND r.claim_id=$2 AND r.state='integrating' AND r.id IN (SELECT id FROM validated_issue_runs) AND s.repository_claimed AND s.current_turn_id IS NULL AND s.status='integrating' RETURNING *")
            .bind(run.id).bind(run.claim_id).bind(serde_json::to_value(policy)?)
            .bind(format!("codex/integration-{}", run.id)).bind(worktree)
            .fetch_one(&mut *tx).await.context("Integration already attempted or validation/session claim is stale")?;
        tx.commit().await?;
        Ok(Some(integration))
    }

    pub async fn integration_source(
        &self,
        run: Uuid,
        default: &str,
        base: &str,
        source: &str,
    ) -> Result<()> {
        let changed = sqlx::query("UPDATE issue_integrations SET default_branch=$2,base_sha=$3,source_sha=$4,updated_at=now() WHERE run_id=$1 AND state='preparing' AND source_sha IS NULL")
            .bind(run).bind(default).bind(base).bind(source).execute(&self.pool).await?.rows_affected();
        ensure!(changed == 1, "Integration source already pinned");
        Ok(())
    }

    pub async fn integration_candidate(
        &self,
        run: Uuid,
        candidate: &str,
        tree: &str,
    ) -> Result<()> {
        let changed = sqlx::query("UPDATE issue_integrations SET candidate_sha=$2,candidate_tree=$3,state='validating',updated_at=now() WHERE run_id=$1 AND state='preparing' AND source_sha IS NOT NULL AND candidate_sha IS NULL")
            .bind(run).bind(candidate).bind(tree).execute(&self.pool).await?.rows_affected();
        ensure!(changed == 1, "Integration candidate already pinned");
        Ok(())
    }

    pub async fn integration_checks(&self, run: Uuid, results: &[CheckResult]) -> Result<()> {
        let changed = sqlx::query("UPDATE issue_integrations SET results=$2,updated_at=now() WHERE run_id=$1 AND state='validating'")
            .bind(run).bind(serde_json::to_value(results)?).execute(&self.pool).await?.rows_affected();
        ensure!(changed == 1, "Integration validation is no longer running");
        Ok(())
    }

    pub async fn validate_integration(&self, run: Uuid) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let i: Integration = sqlx::query_as(
            "SELECT * FROM issue_integrations WHERE run_id=$1 AND state='validating' FOR UPDATE",
        )
        .bind(run)
        .fetch_one(&mut *tx)
        .await?;
        let results: Vec<CheckResult> = serde_json::from_value(i.results)?;
        let policy: validation::Policy = serde_json::from_value(i.validation_policy)?;
        policy.validate()?;
        ensure!(
            validation::decide(&results, 0, 0) == Decision::Passed,
            "Integration build, test and lint did not all pass"
        );
        let changed = sqlx::query("UPDATE issue_integrations i SET state='validated',validated_at=now(),updated_at=now() FROM issue_runs r WHERE i.run_id=$1 AND r.id=i.run_id AND r.validation_id=i.validation_id AND r.state='integrating' AND r.id IN (SELECT id FROM validated_issue_runs)")
            .bind(run).execute(&mut *tx).await?.rows_affected();
        ensure!(changed == 1, "Issue validation was invalidated");
        tx.commit().await?;
        Ok(())
    }

    /// Compare-and-set intent. In particular a caller cannot repeat a push,
    /// create, or merge after an uncertain response using the previous state.
    pub async fn integration_step(&self, run: Uuid, from: &str, to: &str) -> Result<()> {
        ensure!(
            matches!(
                (from, to),
                ("validated", "pushing")
                    | ("pushing", "pushed")
                    | ("pushed", "creating_pr")
                    | ("creating_pr", "checking")
                    | ("checking", "merging")
            ),
            "Invalid integration transition"
        );
        let changed = sqlx::query("UPDATE issue_integrations i SET state=$3,operation=CASE $3 WHEN 'pushing' THEN 'push' WHEN 'creating_pr' THEN 'create_pr' WHEN 'merging' THEN 'merge' ELSE operation END,updated_at=now() FROM issue_runs r WHERE i.run_id=$1 AND i.state=$2 AND i.validated_at IS NOT NULL AND r.id=i.run_id AND r.validation_id=i.validation_id AND r.state='integrating' AND r.id IN (SELECT id FROM validated_issue_runs) AND EXISTS(SELECT 1 FROM issue_release_locks l WHERE l.run_id=i.run_id AND l.repository_id=i.repository_id)")
            .bind(run).bind(from).bind(to).execute(&self.pool).await?.rows_affected();
        ensure!(changed == 1, "Stale integration state or validation");
        Ok(())
    }

    pub async fn integration_pr(&self, run: Uuid, number: i64, url: &str) -> Result<()> {
        let changed = sqlx::query("UPDATE issue_integrations SET pr_number=$2,pr_url=$3,updated_at=now() WHERE run_id=$1 AND (pr_number IS NULL OR pr_number=$2)")
            .bind(run).bind(number).bind(url).execute(&self.pool).await?.rows_affected();
        ensure!(changed == 1, "Integration PR changed");
        Ok(())
    }

    pub async fn integration_merge_request(&self, run: Uuid, response: &Value) -> Result<()> {
        let id = response["details"]["uuid"]
            .as_str()
            .map(str::parse::<Uuid>)
            .transpose()?;
        sqlx::query("UPDATE issue_integrations SET merge_request_id=COALESCE($2,merge_request_id),updated_at=now() WHERE run_id=$1 AND state='merging'")
            .bind(run).bind(id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn finish_integration(&self, run: Uuid, merge: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let owns_slot: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_release_locks WHERE run_id=$1)")
                .bind(run)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(
            owns_slot,
            "Integration/release ownership is missing; inspect before adopting a merge"
        );
        let session: Uuid =
            sqlx::query_scalar("SELECT session_id FROM issue_runs WHERE id=$1 FOR UPDATE")
                .bind(run)
                .fetch_one(&mut *tx)
                .await?;
        let changed = sqlx::query("UPDATE issue_integrations SET state='merged',merge_sha=$2,retry_safe=false,last_error=NULL,updated_at=now() WHERE run_id=$1 AND validated_at IS NOT NULL AND pr_number IS NOT NULL AND (merge_sha IS NULL OR merge_sha=$2)")
            .bind(run).bind(merge).execute(&mut *tx).await?.rows_affected();
        ensure!(changed == 1, "Incomplete integration evidence");
        let changed = sqlx::query("UPDATE issue_runs SET state='releasing',last_error=NULL,updated_at=now() WHERE id=$1 AND state IN ('integrating','blocked','releasing','cancelled') AND id IN (SELECT id FROM integrated_issue_runs)")
            .bind(run).execute(&mut *tx).await?.rows_affected();
        ensure!(changed == 1, "Stale integration or issue validation");
        let reason = format!(
            "Integrated validated issue as {merge}. Release is gated; no deployment was started. Complete the configured release verification before /complete."
        );
        sqlx::query("UPDATE sessions SET status='idle',repository_claimed=false,last_outcome='completed',last_error=NULL,status_snapshot=jsonb_set(status_snapshot,'{activity}',to_jsonb($2::text)),updated_at=now() WHERE id=$1 AND status IN ('idle','integrating') AND current_turn_id IS NULL")
            .bind(session).bind(&reason).execute(&mut *tx).await?;
        persistence::enqueue(
            &mut tx,
            session,
            &format!("integration-merged:{run}"),
            &reason,
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn block_integration(
        &self,
        run: Uuid,
        reason: &str,
        uncertain: bool,
        retry_safe: bool,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE issue_integrations SET state=$2,last_error=$3,retry_safe=$4,updated_at=now() WHERE run_id=$1 AND state <> 'merged'")
            .bind(run).bind(if uncertain { "uncertain" } else { "blocked" }).bind(reason).bind(retry_safe).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM issue_release_locks l USING issue_integrations i WHERE l.run_id=$1 AND i.run_id=l.run_id AND i.state='blocked' AND i.retry_safe AND i.merge_sha IS NULL")
            .bind(run).execute(&mut *tx).await?;
        let session: Option<Uuid> = sqlx::query_scalar("UPDATE issue_runs SET state='blocked',last_error=$2,updated_at=now() WHERE id=$1 AND state NOT IN ('released','cancelled') RETURNING session_id")
            .bind(run).bind(reason).fetch_optional(&mut *tx).await?.flatten();
        if let Some(session) = session {
            sqlx::query("UPDATE sessions SET status='idle',repository_claimed=false,last_error=$2,status_snapshot=jsonb_set(status_snapshot,'{activity}',to_jsonb($2::text)),updated_at=now() WHERE id=$1 AND status='integrating' AND current_turn_id IS NULL")
                .bind(session).bind(reason).execute(&mut *tx).await?;
            persistence::enqueue(
                &mut tx,
                session,
                &format!("integration-blocked:{run}"),
                reason,
                None,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn require_issue_integration(&self, session: Uuid) -> Result<()> {
        let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_runs WHERE session_id=$1 AND (state <> 'releasing' OR id NOT IN (SELECT id FROM integrated_issue_runs)))")
            .bind(session).fetch_one(&self.pool).await?;
        ensure!(
            !blocked,
            "Issue integration is not confirmed. Inspect /queue status; validation alone does not allow /complete"
        );
        Ok(())
    }
}
