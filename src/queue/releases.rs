use crate::{
    persistence::{self, Store},
    releases::{HealthResult, Policy, Release},
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use uuid::Uuid;

impl Store {
    /// The bot requests work; only the separate controller holds dispatch credentials.
    pub async fn request_issue_release(&self, run: Uuid) -> Result<()> {
        let changed = sqlx::query("UPDATE issue_runs r SET release_requested=true,state='releasing',last_error=NULL,updated_at=now()
            WHERE r.id=$1 AND r.state IN ('releasing','blocked') AND r.id IN (SELECT id FROM integrated_issue_runs)
                AND EXISTS(SELECT 1 FROM issue_integrations i WHERE i.run_id=r.id AND i.policy->>'release_gate'='dispatch_only')
                AND EXISTS(SELECT 1 FROM issue_release_locks l WHERE l.run_id=r.id)
                AND NOT EXISTS(SELECT 1 FROM issue_releases d WHERE d.run_id=r.id)")
            .bind(run).execute(&self.pool).await?.rows_affected();
        ensure!(
            changed == 1,
            "Release is already dispatched or lacks verified integration and dispatch_only gating; inspect /queue status"
        );
        Ok(())
    }

    pub async fn recover_releases(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let runs: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT id,session_id FROM issue_runs WHERE state='releasing' ORDER BY id FOR UPDATE",
        )
        .fetch_all(&mut *tx)
        .await?;
        for (run, session) in runs {
            // Recheck after locking: the other process may have committed its
            // dispatch intent while this recovery transaction waited.
            let dispatched: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_releases WHERE run_id=$1)")
                    .bind(run)
                    .fetch_one(&mut *tx)
                    .await?;
            if dispatched {
                continue;
            }
            let reason = "Release was not dispatched before restart. Inspect the recorded merge and trusted configuration, then use /queue release. No deployment was replayed.";
            sqlx::query("UPDATE issue_runs SET state='blocked',release_requested=false,last_error=$2,updated_at=now() WHERE id=$1")
                .bind(run).bind(reason).execute(&mut *tx).await?;
            persistence::enqueue(
                &mut tx,
                session,
                &format!("release-blocked:{run}:configuration"),
                reason,
                None,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn release(&self, run: Uuid) -> Result<Option<Release>> {
        Ok(
            sqlx::query_as("SELECT * FROM issue_releases WHERE run_id=$1")
                .bind(run)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Commit before sending the sole deployment mutation. A persisted row is
    /// never permission to send again, even when it has no workflow run ID.
    pub async fn begin_issue_release(&self, run: Uuid, policy: &Policy) -> Result<Release> {
        policy.validate()?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT id FROM issue_runs WHERE id=$1 FOR UPDATE")
            .bind(run)
            .execute(&mut *tx)
            .await?;
        let release = sqlx::query_as("INSERT INTO issue_releases(run_id,github_repository,expected_sha,policy,deployment_deadline)
            SELECT r.id,r.github_repository,i.merge_sha,$2,now()+make_interval(secs => $3::double precision)
            FROM issue_runs r JOIN issue_integrations i ON i.run_id=r.id JOIN issue_release_locks l ON l.run_id=r.id
            WHERE r.id=$1 AND r.state='releasing' AND r.release_requested AND r.id IN (SELECT id FROM integrated_issue_runs)
                AND i.policy->>'release_gate'='dispatch_only' AND r.github_repository=$4
                AND i.policy->>'github_repository'=$4 AND l.repository_id=r.repository_id
                AND lower(l.github_repository)=lower($4) RETURNING *")
            .bind(run).bind(serde_json::to_value(policy)?).bind(f64::from(policy.deployment_timeout_seconds))
            .bind(&policy.github_repository).fetch_one(&mut *tx).await.context("Release already attempted, integration is unverified, or dispatch gate/ownership is missing")?;
        sqlx::query(
            "UPDATE issue_runs SET release_requested=false,last_error=NULL,updated_at=now() WHERE id=$1",
        )
        .bind(run)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(release)
    }

    pub async fn release_run_id(&self, run: Uuid, workflow: i64) -> Result<()> {
        ensure!(workflow > 0, "Invalid release workflow run ID");
        let changed = sqlx::query("UPDATE issue_releases SET workflow_run_id=$2,state=CASE WHEN state='verifying' THEN state ELSE 'deploying' END,updated_at=now()
            WHERE run_id=$1 AND state IN ('dispatching','deploying','uncertain','verifying') AND (workflow_run_id IS NULL OR workflow_run_id=$2)")
            .bind(run).bind(workflow).execute(&self.pool).await?.rows_affected();
        ensure!(changed == 1, "Release workflow identity/state changed");
        Ok(())
    }

    pub async fn observe_release_workflow(&self, run: Uuid, observation: &Value) -> Result<()> {
        let release = self.release(run).await?.context("Missing release intent")?;
        let policy: Policy = serde_json::from_value(release.policy.clone())?;
        policy.verify_run(&release, observation)?;
        let successful =
            observation["status"] == "completed" && observation["conclusion"] == "success";
        let changed = sqlx::query("UPDATE issue_releases SET workflow_run_id=$2,workflow_observation=$3,
            state=CASE WHEN $4 THEN 'verifying' ELSE 'deploying' END,
            health_deadline=CASE WHEN $4 THEN COALESCE(health_deadline,now()+make_interval(secs => $5::double precision)) ELSE health_deadline END,
            last_error=NULL,updated_at=now() WHERE run_id=$1 AND state IN ('dispatching','deploying','uncertain','verifying')
            AND (workflow_run_id IS NULL OR workflow_run_id=$2) AND (health_deadline IS NOT NULL OR deployment_deadline>now())")
            .bind(run).bind(observation["id"].as_i64()).bind(observation).bind(successful)
            .bind(f64::from(policy.health_timeout_seconds)).execute(&self.pool).await?.rows_affected();
        ensure!(
            changed == 1,
            "Release observation is stale or deployment deadline expired"
        );
        Ok(())
    }

    pub async fn record_release_health(&self, run: Uuid, results: &[HealthResult]) -> Result<()> {
        let observed = results
            .first()
            .and_then(|r| r.revision.as_deref())
            .filter(|sha| {
                crate::releases::is_sha(sha)
                    && results.iter().all(|r| r.revision.as_deref() == Some(sha))
            });
        let changed = sqlx::query("UPDATE issue_releases SET health_results=$2,deployed_revision=$3,updated_at=now() WHERE run_id=$1 AND state='verifying' AND health_deadline>now()")
            .bind(run).bind(serde_json::to_value(results)?).bind(observed).execute(&self.pool).await?.rows_affected();
        ensure!(
            changed == 1,
            "Health verification is no longer running or timed out"
        );
        Ok(())
    }

    pub async fn block_release(&self, run: Uuid, reason: &str, uncertain: bool) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let session: Option<Uuid> = sqlx::query_scalar("UPDATE issue_runs SET state='blocked',release_requested=false,last_error=$2,updated_at=now()
            WHERE id=$1 AND state IN ('releasing','blocked') AND id IN (SELECT id FROM integrated_issue_runs) RETURNING session_id")
            .bind(run).bind(reason).fetch_optional(&mut *tx).await?.flatten();
        if let Some(session) = session {
            let has_intent: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_releases WHERE run_id=$1)")
                    .bind(run)
                    .fetch_one(&mut *tx)
                    .await?;
            sqlx::query("UPDATE issue_releases SET state=$2,last_error=$3,updated_at=now() WHERE run_id=$1 AND state NOT IN ('succeeded','failed')")
                .bind(run).bind(if uncertain { "uncertain" } else { "failed" }).bind(reason).execute(&mut *tx).await?;
            sqlx::query("UPDATE sessions SET last_error=$2,status_snapshot=jsonb_set(status_snapshot,'{activity}',to_jsonb($2::text)),updated_at=now() WHERE id=$1")
                .bind(session).bind(reason).execute(&mut *tx).await?;
            persistence::enqueue(
                &mut tx,
                session,
                &format!(
                    "release-blocked:{run}:{}",
                    if !has_intent {
                        "configuration"
                    } else if uncertain {
                        "uncertain"
                    } else {
                        "failed"
                    }
                ),
                reason,
                None,
            )
            .await?;
        }
        // The durable integration/release lock intentionally survives failure.
        tx.commit().await?;
        Ok(())
    }

    pub async fn finish_issue_release(
        &self,
        run: Uuid,
        retention: std::time::Duration,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let session: Uuid =
            sqlx::query_scalar("SELECT session_id FROM issue_runs WHERE id=$1 FOR UPDATE")
                .bind(run)
                .fetch_one(&mut *tx)
                .await?;
        let release: Release =
            sqlx::query_as("SELECT * FROM issue_releases WHERE run_id=$1 FOR UPDATE")
                .bind(run)
                .fetch_one(&mut *tx)
                .await?;
        if release.state == "succeeded" {
            return Ok(());
        }
        ensure!(
            release.state == "verifying"
                && release
                    .health_deadline
                    .is_some_and(|d| d > chrono::Utc::now()),
            "Release has no current health verification"
        );
        let policy: Policy = serde_json::from_value(release.policy.clone())?;
        policy.validate()?;
        policy.verify_run(&release, &release.workflow_observation)?;
        ensure!(
            release.workflow_observation["status"] == "completed"
                && release.workflow_observation["conclusion"] == "success",
            "Deployment workflow did not finish successfully"
        );
        policy.verify_health(
            &release.expected_sha,
            &serde_json::from_value::<Vec<HealthResult>>(release.health_results.clone())?,
        )?;
        let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM issue_runs r JOIN issue_integrations i ON i.run_id=r.id JOIN issue_release_locks l ON l.run_id=r.id
            WHERE r.id=$1 AND r.state IN ('releasing','blocked') AND r.id IN (SELECT id FROM integrated_issue_runs)
                AND i.merge_sha=$2 AND r.github_repository=$3 AND l.repository_id=r.repository_id AND lower(l.github_repository)=lower($3))")
            .bind(run).bind(&release.expected_sha).bind(&release.github_repository).fetch_one(&mut *tx).await?;
        ensure!(
            valid,
            "Integration/release ownership or expected revision changed"
        );
        sqlx::query("UPDATE issue_releases SET state='succeeded',deployed_revision=expected_sha,verified_at=now(),last_error=NULL,updated_at=now() WHERE run_id=$1")
            .bind(run).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO issue_release_confirmations(run_id,github_repository,merge_sha,confirmed_by,verification) VALUES($1,$2,$3,'release-controller','controller')")
            .bind(run).bind(&release.github_repository).bind(&release.expected_sha).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO issue_worktree_cleanup(run_id,kind,path,branch,revision) SELECT r.id,'issue',r.worktree,r.branch,i.source_sha FROM issue_runs r JOIN issue_integrations i ON i.run_id=r.id WHERE r.id=$1 AND r.worktree IS NOT NULL AND r.branch IS NOT NULL AND i.source_sha IS NOT NULL UNION ALL SELECT run_id,'integration',worktree,branch,candidate_sha FROM issue_integrations WHERE run_id=$1 AND candidate_sha IS NOT NULL")
            .bind(run).execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE issue_runs SET state='released',last_error=NULL,updated_at=now() WHERE id=$1",
        )
        .bind(run)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM issue_release_locks WHERE run_id=$1 AND run_id IN (SELECT id FROM confirmed_issue_releases)")
            .bind(run).execute(&mut *tx).await?;
        let reason = format!(
            "Released and independently verified healthy revision {}. Workflow: https://github.com/{}/actions/runs/{}. Task completed; channel retention and clean-worktree cleanup are scheduled.",
            release.expected_sha,
            release.github_repository,
            release.workflow_run_id.context("Missing workflow ID")?
        );
        sqlx::query("UPDATE sessions SET channel_state=CASE WHEN channel_kind='text' THEN 'completed' ELSE channel_state END,
            completed_at=now(),delete_after=CASE WHEN channel_kind='text' THEN now()+make_interval(secs => $2::double precision) ELSE NULL END,
            last_error=NULL,status_snapshot=jsonb_set(status_snapshot,'{activity}',to_jsonb($3::text)),updated_at=now() WHERE id=$1")
            .bind(session).bind(retention.as_secs_f64()).bind(&reason).execute(&mut *tx).await?;
        persistence::enqueue(
            &mut tx,
            session,
            &format!("release-succeeded:{run}"),
            &reason,
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn require_issue_release(&self, session: Uuid) -> Result<()> {
        let blocked: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM issue_runs WHERE session_id=$1 AND
            (state <> 'released' OR id NOT IN (SELECT id FROM confirmed_issue_releases)))",
        )
        .bind(session)
        .fetch_one(&self.pool)
        .await?;
        ensure!(
            !blocked,
            "Release has not been independently verified. Inspect /queue status; /complete cannot certify deployment health"
        );
        Ok(())
    }
}
