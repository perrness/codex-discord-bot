use crate::{
    persistence::{self, Store},
    queue::{Run, State},
    validation::{self, CheckResult, Decision, Policy},
};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use uuid::Uuid;

impl Store {
    pub(crate) async fn cancel_finished_validation(
        &self,
        run: &Run,
        validation: Uuid,
        source: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE issue_runs SET state='blocked',validation_id=NULL,last_error='Validation cancelled',updated_at=now() WHERE id=$1 AND claim_id=$2")
            .bind(run.id).bind(run.claim_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE issue_validations SET state='blocked',error='Validation cancelled' WHERE id=$1 AND run_id=$2")
            .bind(validation).bind(run.id).execute(&mut *tx).await?;
        sqlx::query("UPDATE inbound_requests SET state='rejected',updated_at=now() WHERE discord_id=$1 AND state='received'")
            .bind(source).execute(&mut *tx).await?;
        sqlx::query("UPDATE sessions SET status='idle',repository_claimed=false,last_error='Validation cancelled',updated_at=now() WHERE id=$1 AND current_turn_id IS NULL")
            .bind(run.session_id).execute(&mut *tx).await?;
        if let Some(session) = run.session_id {
            persistence::enqueue(
                &mut tx,
                session,
                &format!("validation-cancelled:{validation}"),
                "Validation cancelled. No repair was submitted; integration is blocked.",
                None,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn begin_issue_validation(
        &self,
        run: &Run,
        turn: &str,
        configured: Option<&Policy>,
    ) -> Result<(Uuid, Option<Policy>)> {
        let id = Uuid::new_v4();
        let mut tx = self.pool.begin().await?;
        let policy: Option<Value> = sqlx::query_scalar("UPDATE issue_runs SET validation_policy=COALESCE(validation_policy,$3),updated_at=now() WHERE id=$1 AND claim_id=$2 AND state='validating' AND validation_id IS NULL RETURNING validation_policy")
            .bind(run.id).bind(run.claim_id).bind(configured.map(serde_json::to_value).transpose()?)
            .fetch_one(&mut *tx).await.context("Stale validation claim")?;
        sqlx::query("INSERT INTO issue_validations(id,run_id,turn_id,policy) VALUES($1,$2,$3,$4)")
            .bind(id)
            .bind(run.id)
            .bind(turn)
            .bind(&policy)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE issue_runs SET validation_id=$2 WHERE id=$1")
            .bind(run.id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((id, policy.map(serde_json::from_value).transpose()?))
    }

    pub async fn record_issue_checks(
        &self,
        validation: Uuid,
        results: &[CheckResult],
    ) -> Result<()> {
        let changed =
            sqlx::query("UPDATE issue_validations SET results=$2 WHERE id=$1 AND state='running'")
                .bind(validation)
                .bind(serde_json::to_value(results)?)
                .execute(&self.pool)
                .await?
                .rows_affected();
        ensure!(changed == 1, "Validation is no longer running");
        Ok(())
    }

    /// Returns a durably reserved repair submission, or releases the session claim.
    /// This method consumes only worker evidence stored by the Rust orchestrator.
    pub async fn finish_issue_validation(
        &self,
        run: &Run,
        validation: Uuid,
        error: Option<&str>,
    ) -> Result<Option<(String, String)>> {
        let mut tx = self.pool.begin().await?;
        let (repairs, session): (i32, Uuid) = sqlx::query_as("SELECT repair_count,session_id FROM issue_runs WHERE id=$1 AND claim_id=$2 AND state='validating' AND validation_id=$3 FOR UPDATE")
            .bind(run.id).bind(run.claim_id).bind(validation).fetch_one(&mut *tx).await.context("Stale validation claim")?;
        let (policy, results): (Option<Value>, Value) = sqlx::query_as("SELECT policy,results FROM issue_validations WHERE id=$1 AND run_id=$2 AND state='running' FOR UPDATE")
            .bind(validation).bind(run.id).fetch_one(&mut *tx).await?;
        let results: Vec<CheckResult> = serde_json::from_value(results)?;
        let policy: Option<Policy> = policy.map(serde_json::from_value).transpose()?;
        let decision = match &policy {
            Some(policy) if error.is_none() && policy.validate().is_ok() => {
                validation::decide(&results, repairs as u32, policy.max_repairs)
            }
            _ => Decision::Blocked,
        };
        let reason = match decision {
            Decision::Passed => "Trusted build, test and lint checks passed. Preparing a fresh integration candidate.".to_owned(),
            Decision::Repair => format!("Trusted validation failed. Starting repair {} of {} in this issue's conversation.", repairs + 1, policy.as_ref().context("Missing repair policy")?.max_repairs),
            Decision::Blocked => error.map(str::to_owned).unwrap_or_else(||
                "Validation blocked: checks failed, were incomplete/cancelled, or the repair limit was exhausted. Inspect the recorded results; dismiss and explicitly retry the issue after review.".into()),
        };
        sqlx::query("UPDATE issue_validations SET state=$2,error=$3,finished_at=now() WHERE id=$1")
            .bind(validation)
            .bind(match decision {
                Decision::Passed => "passed",
                Decision::Repair => "failed",
                Decision::Blocked => "blocked",
            })
            .bind(if decision == Decision::Passed {
                None
            } else {
                Some(reason.as_str())
            })
            .execute(&mut *tx)
            .await?;
        let repair = if decision == Decision::Repair {
            let limit = policy
                .as_ref()
                .context("Missing repair policy")?
                .max_repairs;
            let changed = sqlx::query("UPDATE issue_runs SET state='implementing',repair_count=repair_count+1,validation_id=NULL,last_error=$3,updated_at=now() WHERE id=$1 AND claim_id=$2 AND repair_count < $4")
                .bind(run.id).bind(run.claim_id).bind(&reason).bind(limit as i32).execute(&mut *tx).await?.rows_affected();
            ensure!(changed == 1, "Repair budget exhausted");
            let source = format!("issue-repair:{}:{}", run.id, repairs + 1);
            sqlx::query("INSERT INTO inbound_requests(discord_id,session_id,kind,state) VALUES($1,$2,'issue-repair','received')")
                .bind(&source).bind(session).execute(&mut *tx).await?;
            Some((
                source,
                validation::repair_prompt(&results, repairs as u32 + 1, limit),
            ))
        } else {
            let next = if decision == Decision::Passed {
                State::Integrating
            } else {
                State::Blocked
            };
            let changed = sqlx::query("UPDATE issue_runs SET state=$3,last_error=$4,updated_at=now() WHERE id=$1 AND claim_id=$2 AND ($3 <> 'integrating' OR id IN (SELECT id FROM validated_issue_runs))")
                .bind(run.id).bind(run.claim_id).bind(next.as_str())
                .bind(if decision == Decision::Passed { None } else { Some(reason.as_str()) }).execute(&mut *tx).await?.rows_affected();
            ensure!(changed == 1, "Missing successful validation evidence");
            sqlx::query("UPDATE sessions SET status=$3,repository_claimed=$4,last_error=$2,updated_at=now() WHERE id=$1 AND status='validating' AND current_turn_id IS NULL")
                .bind(session).bind(if decision == Decision::Passed { None } else { Some(reason.as_str()) })
                .bind(if decision == Decision::Passed { "integrating" } else { "idle" }).bind(decision == Decision::Passed).execute(&mut *tx).await?;
            None
        };
        sqlx::query("UPDATE sessions SET status_snapshot=jsonb_set(status_snapshot,'{activity}',to_jsonb($2::text)),updated_at=now() WHERE id=$1")
            .bind(session).bind(&reason).execute(&mut *tx).await?;
        persistence::enqueue(
            &mut tx,
            session,
            &format!("validation:{validation}"),
            &reason,
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(repair)
    }
}
