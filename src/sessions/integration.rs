use super::*;
use crate::{
    integration::{Integration, Policy, git, github::GitHub},
    queue::Run,
    validation::{self, Outcome},
};
use std::path::Path;

impl Manager {
    // Called under the session gate, retaining its existing repository/file claim.
    pub(super) async fn integrate_issue(&self, session: &Session, run: &Run) -> Result<()> {
        let cancel = self.fatal.child_token();
        self.validations
            .lock()
            .unwrap()
            .insert(session.id, cancel.clone());
        let result = self.integrate_issue_locked(session, run, &cancel).await;
        self.validations.lock().unwrap().remove(&session.id);
        if let Err(error) = result {
            let reason = format!("Integration blocked: {error:#}");
            if let Some(i) = self.store.integration(run.id).await? {
                // Read back ambiguous mutations immediately. Never repeat them.
                if i.operation.is_some() {
                    if self.reconcile_integration(&i).await.is_ok() {
                        return Ok(());
                    }
                    self.store
                        .block_integration(run.id, &reason, true, false)
                        .await?;
                } else {
                    self.store
                        .block_integration(run.id, &reason, false, true)
                        .await?;
                }
            } else {
                self.store
                    .block_integration(run.id, &reason, false, true)
                    .await?;
            }
        }
        Ok(())
    }

    async fn integrate_issue_locked(
        &self,
        session: &Session,
        run: &Run,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let repo: Repository =
            sqlx::query_as("SELECT id,name,path,github_repository FROM repositories WHERE id=$1")
                .bind(run.repository_id)
                .fetch_one(&self.store.pool)
                .await?;
        let policy = self
            .config
            .integration
            .repositories
            .get(&repo.name)
            .context("No trusted INTEGRATION_CONFIG/release gate for this repository")?;
        self.validate_session_directory(&repo, session).await?;
        let worktree = Path::new(&session.working_directory)
            .parent()
            .context("Missing worktree parent")?
            .join(format!("integration-{}", run.id));
        let i = self
            .store
            .begin_issue_integration(
                run,
                policy,
                worktree.to_str().context("Invalid worktree path")?,
            )
            .await?;
        let gh = GitHub::new(&policy.github_repository)?;
        ensure!(
            crate::github::origin(Path::new(&repo.path)).await?.as_str()
                == policy.github_repository,
            "Repository origin differs from trusted integration target"
        );
        ensure!(!cancel.is_cancelled(), "Integration cancelled");
        let default = gh.default_branch().await?;
        gh.protection(&default).await?;
        let base = git::fetch(
            Path::new(&repo.path),
            &gh.remote(),
            &format!("refs/heads/{default}"),
        )
        .await?;
        let source = git::snapshot(Path::new(&session.working_directory), run.issue_number).await?;
        self.store
            .integration_source(run.id, &default, &base, &source)
            .await?;
        let (candidate, tree) =
            git::candidate(Path::new(&repo.path), &worktree, &i.branch, &base, &source).await?;
        self.store
            .integration_candidate(run.id, &candidate, &tree)
            .await?;
        git::release_gate(&worktree, &base, &candidate, policy).await?;
        let worker: validation::Policy = serde_json::from_value(i.validation_policy)?;
        let (verify, config) = (worker.clone(), self.config.clone());
        tokio::task::spawn_blocking(move || {
            verify.verify_worker(&[
                &config.repos_root,
                &config.state_dir,
                &config.codex_home,
                Path::new("/etc"),
                Path::new("/home"),
                Path::new("/root"),
                Path::new("/run"),
            ])
        })
        .await??;
        let mut results = Vec::new();
        for (name, check) in worker.checks() {
            self.store
                .activity(
                    session.id,
                    &format!("Validating integration candidate: {name}"),
                )
                .await?;
            let result = validation::run_check(&worker, &worktree, name, check, cancel).await;
            let stop = matches!(result.outcome, Outcome::Error | Outcome::Cancelled);
            results.push(result);
            self.store.integration_checks(run.id, &results).await?;
            if stop {
                break;
            }
        }
        ensure!(!cancel.is_cancelled(), "Integration cancelled");
        crate::repositories::validate_issue_worktree(Path::new(&repo.path), &worktree, &i.branch)
            .await?;
        git::clean(&worktree, &candidate).await?;
        git::clean(Path::new(&session.working_directory), &source).await?;
        self.store.validate_integration(run.id).await?;
        let i = self
            .store
            .integration(run.id)
            .await?
            .context("Missing integration")?;
        gh.fresh(&i).await?;
        ensure!(!cancel.is_cancelled(), "Integration cancelled before push");
        self.store
            .integration_step(run.id, "validated", "pushing")
            .await?;
        let pushed = git::push(&worktree, &gh.remote(), &candidate, &i.branch).await;
        // A failed client response can still have pushed exactly the expected ref.
        let remote = gh.branch_sha(&i.branch).await;
        if remote.as_deref().ok() != Some(candidate.as_str()) {
            pushed?;
            bail!("Integration branch push could not be confirmed");
        }
        self.store
            .integration_step(run.id, "pushing", "pushed")
            .await?;
        ensure!(
            !cancel.is_cancelled(),
            "Integration cancelled before PR creation"
        );
        gh.fresh(&i).await?;
        let existing = gh.find_pr(&i).await?;
        self.store
            .integration_step(run.id, "pushed", "creating_pr")
            .await?;
        let pr = if let Some(pr) = existing {
            pr
        } else {
            gh.create_or_reconcile_pr(&i, run.issue_number).await?
        };
        self.remember_pr(&gh, &i, &pr).await?;
        self.store
            .integration_step(run.id, "creating_pr", "checking")
            .await?;
        let i = self
            .store
            .integration(run.id)
            .await?
            .context("Missing integration")?;
        gh.status(&i).await?;
        self.store
            .activity(
                session.id,
                "Waiting for GitHub required checks and branch protection",
            )
            .await?;
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(policy.checks_timeout_seconds);
        loop {
            ensure!(!cancel.is_cancelled(), "Integration cancelled before merge");
            let pr = gh.pr(i.pr_number.context("Missing PR")?).await?;
            gh.identity(&pr, &i)?;
            if pr["merged"] == true {
                return self.confirm_merge(&gh, &i, &pr).await;
            }
            ensure!(
                pr["state"] == "open",
                "Integration PR was closed without merging"
            );
            gh.fresh(&i).await?;
            ensure!(
                pr["mergeable_state"] != "dirty",
                "GitHub reports merge conflicts"
            );
            if pr["mergeable"] == true && pr["mergeable_state"] == "clean" {
                break;
            }
            wait_for_checks(deadline, cancel).await?;
        }
        // Protection's strict flag guards advancement *after* these observations.
        gh.fresh(&i).await?;
        ensure!(
            gh.branch_sha(&i.branch).await? == candidate,
            "Integration head advanced; validation is stale"
        );
        git::clean(&worktree, &candidate).await?;
        ensure!(!cancel.is_cancelled(), "Integration cancelled before merge");
        self.store
            .integration_step(run.id, "checking", "merging")
            .await?;
        let response = gh.merge(&i).await?;
        self.store
            .integration_merge_request(run.id, &response)
            .await?;
        let i = self
            .store
            .integration(run.id)
            .await?
            .context("Missing integration")?;
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(policy.checks_timeout_seconds);
        loop {
            let pr = gh.pr(i.pr_number.context("Missing PR")?).await?;
            if pr["merged"] == true {
                return self.confirm_merge(&gh, &i, &pr).await;
            }
            gh.identity(&pr, &i)?;
            if let Some(request) = i.merge_request_id {
                let response = gh
                    .api(
                        "GET",
                        &format!("/pulls/{}/merge-async/{request}", i.pr_number.unwrap()),
                        None,
                    )
                    .await?;
                ensure!(
                    response["status"] != "failed" && response["status"] != "enqueued",
                    "GitHub did not merge the candidate; protection or merge queue requires attention"
                );
            }
            wait_for_checks(deadline, cancel).await?;
        }
    }

    async fn remember_pr(&self, gh: &GitHub, i: &Integration, pr: &Value) -> Result<()> {
        gh.identity(pr, i)?;
        let number = pr["number"].as_i64().context("Missing PR number")?;
        let policy: Policy = serde_json::from_value(i.policy.clone())?;
        self.store
            .integration_pr(
                i.run_id,
                number,
                &format!(
                    "https://github.com/{}/pull/{number}",
                    policy.github_repository
                ),
            )
            .await
    }

    async fn confirm_merge(&self, gh: &GitHub, i: &Integration, pr: &Value) -> Result<()> {
        // Retain the observed SHA even if its tree/parents fail verification.
        if let Some(sha) = pr["merge_commit_sha"].as_str() {
            sqlx::query("UPDATE issue_integrations SET merge_sha=$2,updated_at=now() WHERE run_id=$1 AND merge_sha IS NULL")
                .bind(i.run_id).bind(sha).execute(&self.store.pool).await?;
        }
        let sha = gh.verify_merge(i, pr).await?;
        self.store.finish_integration(i.run_id, &sha).await
    }

    /// Read-only GitHub reconciliation. Never restarts a check, turn or mutation.
    pub(super) async fn reconcile_integration(&self, i: &Integration) -> Result<()> {
        if i.state == "merged" {
            return Ok(());
        }
        if i.operation.is_none() {
            return self.store.block_integration(i.run_id, "Integration interrupted before publishing; checks were not replayed. Dismiss and explicitly retry after inspection.", false, true).await;
        }
        let policy: Policy = serde_json::from_value(i.policy.clone())?;
        let gh = GitHub::new(&policy.github_repository)?;
        let pr = match i.pr_number {
            Some(n) => Some(gh.pr(n).await?),
            None => gh.find_pr(i).await?,
        };
        if let Some(pr) = pr {
            self.remember_pr(&gh, i, &pr).await?;
            let current = self
                .store
                .integration(i.run_id)
                .await?
                .context("Missing integration")?;
            if pr["merged"] == true {
                return self.confirm_merge(&gh, &current, &pr).await;
            }
            let closed = pr["state"] == "closed" && pr["merged"] == false;
            self.store.block_integration(i.run_id, if closed {
                "Integration PR is closed and unmerged. Dismiss the session before explicitly retrying."
            } else { "Integration PR is still open or uncertain. No mutation was replayed. Inspect GitHub and close an unmerged PR before retrying." }, !closed, closed).await
        } else {
            let safe = i.operation.as_deref() == Some("push");
            self.store.block_integration(i.run_id, if safe {
                "Integration stopped after branch publication, before PR creation. No merge was attempted; inspect, dismiss and explicitly retry."
            } else { "PR creation is uncertain and no PR was found. Inspect GitHub before an operator clears the attempt; creation was not replayed." }, !safe, safe).await
        }
    }

    pub async fn reconcile_integrations(&self) -> Result<()> {
        let integrations: Vec<Integration> = sqlx::query_as("SELECT i.* FROM issue_integrations i JOIN issue_runs r ON r.id=i.run_id WHERE i.state <> 'merged' AND r.state NOT IN ('released','cancelled')")
            .fetch_all(&self.store.pool).await?;
        for i in integrations {
            if let Err(error) = self.reconcile_integration(&i).await {
                self.store
                    .block_integration(
                        i.run_id,
                        &format!("Integration reconciliation needs review: {error:#}"),
                        true,
                        false,
                    )
                    .await?;
            }
        }
        Ok(())
    }

    pub(super) async fn reconcile_uncertain_integrations(&self) -> Result<()> {
        let pending: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT i.run_id,r.session_id FROM issue_integrations i JOIN issue_runs r ON r.id=i.run_id WHERE i.state='uncertain' AND r.state='blocked'")
            .fetch_all(&self.store.pool).await?;
        for (run, session) in pending {
            let gate = self.gate(session);
            let Ok(_guard) = gate.try_lock() else {
                continue;
            };
            let i = self
                .store
                .integration(run)
                .await?
                .context("Missing integration")?;
            if i.state == "uncertain"
                && let Err(error) = self.reconcile_integration(&i).await
            {
                // A read outage never licenses a new remote mutation or releases
                // the queue slot. Keep the evidence for the next read-only poll.
                tracing::warn!(%run, %error, "Integration reconciliation deferred");
            }
        }
        Ok(())
    }
}

async fn wait_for_checks(deadline: tokio::time::Instant, cancel: &CancellationToken) -> Result<()> {
    ensure!(
        tokio::time::Instant::now() < deadline,
        "GitHub checks/merge timed out; reconcile before retrying"
    );
    tokio::select! {
        _ = cancel.cancelled() => bail!("Integration cancelled; a submitted merge must still be reconciled"),
        _ = tokio::time::sleep_until((tokio::time::Instant::now() + Duration::from_secs(3)).min(deadline)) => Ok(()),
    }
}
