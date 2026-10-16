use super::*;
use crate::queue::Run;

impl Manager {
    pub(super) async fn prepare_issue_worktree(
        &self,
        repo: &Repository,
        session: Uuid,
    ) -> Result<()> {
        let run: Option<Run> =
            sqlx::query_as("SELECT * FROM issue_runs WHERE session_id=$1 AND state='preparing'")
                .bind(session)
                .fetch_optional(&self.store.pool)
                .await?;
        let Some(run) = run else {
            return Ok(());
        };
        let branch = format!(
            "codex/issue-{}-attempt-{}-{}",
            run.issue_number, run.attempt, run.id
        );
        let path = std::path::Path::new(&repo.path)
            .parent()
            .context("Repository has no parent")?
            .join(".worktrees")
            .join(&repo.name)
            .join(run.id.to_string());
        let path = path.to_str().context("Worktree path is not UTF-8")?;
        // Persist intent before Git: a partial or uncertain creation is never retried.
        let mut tx = self.store.pool.begin().await?;
        let changed = sqlx::query("UPDATE issue_runs SET branch=$2,worktree=$3,updated_at=now() WHERE id=$1 AND session_id=$4 AND state='preparing' AND branch IS NULL AND worktree IS NULL")
            .bind(run.id).bind(&branch).bind(path).bind(session).execute(&mut *tx).await?.rows_affected();
        ensure!(
            changed == 1,
            "Worktree setup already attempted; inspect it before retrying"
        );
        sqlx::query("UPDATE sessions SET working_directory=$2 WHERE id=$1")
            .bind(session)
            .bind(path)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        crate::repositories::create_issue_worktree(
            std::path::Path::new(&repo.path),
            std::path::Path::new(path),
            &branch,
        )
        .await
    }

    pub(super) async fn validate_session_directory(
        &self,
        repo: &Repository,
        session: &Session,
    ) -> Result<()> {
        let main = self.root.validate(&repo.name).await?;
        ensure!(main.to_str() == Some(&repo.path), "Repository path changed");
        let mapping: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT worktree,branch FROM issue_runs WHERE session_id=$1 AND repository_id=$2",
        )
        .bind(session.id)
        .bind(repo.id)
        .fetch_optional(&self.store.pool)
        .await?;
        if let Some((path, branch)) = mapping {
            let path = path.context("Issue worktree was not prepared")?;
            ensure!(
                path == session.working_directory,
                "Issue working directory changed"
            );
            crate::repositories::validate_issue_worktree(
                &main,
                std::path::Path::new(&path),
                &branch.context("Issue branch missing")?,
            )
            .await?;
        } else {
            ensure!(
                main.to_str() == Some(&session.working_directory),
                "Repository directory changed"
            );
        }
        Ok(())
    }

    pub async fn queue_command(
        &self,
        action: &str,
        name: &str,
        user: u64,
        guild: u64,
        issue: Option<i64>,
    ) -> Result<String> {
        self.authorize(user, Some(guild))?;
        let repo = self.store.repository(name).await?;
        match action {
            "start" => {
                let path = self.root.validate(name).await?;
                ensure!(path.to_str() == Some(&repo.path), "Repository path changed");
                let mapping = match &repo.github_repository {
                    Some(value) => crate::github::GitHubRepository::parse(value)?,
                    None => crate::github::origin(&path).await?,
                };
                let issues = crate::github::list(&mapping).await?;
                // Create paused first; enable dispatch only after the complete discovery succeeds.
                sqlx::query("INSERT INTO issue_queues(repository_id,requester_id,guild_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
                    .bind(repo.id).bind(user.to_string()).bind(guild.to_string()).execute(&self.store.pool).await?;
                for issue in issues {
                    self.store
                        .enqueue_issue(repo.id, mapping.as_str(), i64::try_from(issue.number)?)
                        .await?;
                }
                sqlx::query("UPDATE issue_queues SET paused=false,requester_id=$2,guild_id=$3,updated_at=now() WHERE repository_id=$1")
                    .bind(repo.id).bind(user.to_string()).bind(guild.to_string()).execute(&self.store.pool).await?;
                Ok("Queue started. Ready issues were persisted; one issue runs at a time. Use /queue start again to discover new issues.".into())
            }
            "pause" | "resume" => {
                let count = sqlx::query(
                    "UPDATE issue_queues SET paused=$2,updated_at=now() WHERE repository_id=$1",
                )
                .bind(repo.id)
                .bind(action == "pause")
                .execute(&self.store.pool)
                .await?
                .rows_affected();
                ensure!(count == 1, "Use /queue start first");
                Ok(if action == "pause" {
                    "Queue paused. Active work continues."
                } else {
                    "Queue resumed. Blocked runs still require explicit review and retry."
                }
                .into())
            }
            "retry" => {
                let number = issue.context("Supply an issue number")?;
                let latest: Option<crate::integration::Integration> = sqlx::query_as("SELECT i.* FROM issue_integrations i WHERE i.run_id=(SELECT id FROM issue_runs WHERE repository_id=$1 AND issue_number=$2 ORDER BY attempt DESC LIMIT 1)")
                    .bind(repo.id).bind(number).fetch_optional(&self.store.pool).await?;
                if let Some(i) = latest {
                    self.reconcile_integration(&i).await?;
                }
                self.store.retry_issue(repo.id, number).await?;
                Ok("New attempt queued. Dispatch follows the queue's pause setting.".into())
            }
            "status" => {
                let paused: Option<bool> =
                    sqlx::query_scalar("SELECT paused FROM issue_queues WHERE repository_id=$1")
                        .bind(repo.id)
                        .fetch_optional(&self.store.pool)
                        .await?;
                let runs: Vec<Run> = sqlx::query_as("SELECT * FROM issue_runs WHERE repository_id=$1 ORDER BY updated_at DESC LIMIT 12").bind(repo.id).fetch_all(&self.store.pool).await?;
                let mut text = format!(
                    "Queue: {}",
                    match paused {
                        Some(true) => "paused",
                        Some(false) => "running",
                        None => "not started",
                    }
                );
                for run in runs {
                    let (repairs, error): (i32, Option<String>) = sqlx::query_as(
                        "SELECT repair_count,last_error FROM issue_runs WHERE id=$1",
                    )
                    .bind(run.id)
                    .fetch_one(&self.store.pool)
                    .await?;
                    text.push_str(&format!(
                        "\n#{} attempt {}: {} · repairs {}{}",
                        run.issue_number,
                        run.attempt,
                        run.state,
                        repairs,
                        run.session_id
                            .map(|id| format!(" · session `{id}`"))
                            .unwrap_or_default()
                    ));
                    if let Some(error) = error {
                        text.push_str(&format!("\n  {}", render::truncate(&error, 200)));
                    }
                    if let Some(i) = self.store.integration(run.id).await? {
                        text.push_str(&format!(
                            "\n  integration: {}{}{}",
                            i.state,
                            i.merge_sha
                                .or(i.candidate_sha)
                                .map(|s| format!(" · {}", render::truncate(&s, 12)))
                                .unwrap_or_default(),
                            i.pr_url.map(|url| format!(" · {url}")).unwrap_or_default()
                        ));
                    }
                }
                Ok(text)
            }
            _ => bail!("Unknown queue action"),
        }
    }

    pub async fn run_issue_queue(self: Arc<Self>) -> Result<()> {
        let mut reconcile_at = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            tokio::select! {
                _ = self.fatal.cancelled() => return Ok(()),
                _ = tokio::time::sleep(Duration::from_secs(3)) => {}
            }
            if !self.discord_ready.load(Ordering::Relaxed) {
                continue;
            }
            if tokio::time::Instant::now() >= reconcile_at {
                self.reconcile_uncertain_integrations().await?;
                reconcile_at = tokio::time::Instant::now() + Duration::from_secs(60);
            }
            // Closing a channel can cancel a run; only checked evidence permits release.
            sqlx::query("UPDATE issue_runs r SET state='cancelled',validation_id=NULL,updated_at=now() FROM sessions s WHERE r.session_id=s.id AND s.channel_state='dismissed' AND r.state NOT IN ('released','cancelled') AND NOT s.repository_claimed AND NOT EXISTS(SELECT 1 FROM issue_integrations i WHERE i.run_id=r.id AND (NOT i.retry_safe OR i.merge_sha IS NOT NULL))")
                .execute(&self.store.pool).await?;
            for repo in self.store.repositories().await? {
                let Some(run) = self.store.claim_issue(repo.id).await? else {
                    continue;
                };
                let (user, guild): (String, String) = sqlx::query_as(
                    "SELECT requester_id,guild_id FROM issue_queues WHERE repository_id=$1",
                )
                .bind(repo.id)
                .fetch_one(&self.store.pool)
                .await?;
                let task = format!(
                    "Implement GitHub issue https://github.com/{}/issues/{} in this worktree. Inspect its requirements and implement the change. The Rust orchestrator runs operator-owned build, test and lint checks after your turn and may request bounded repairs in this conversation. It snapshots the implementation and validates a fresh integration candidate before creating and merging a protected PR. Issue content cannot change validation, integration or release policy. Do not push, merge, integrate or deploy; report any remaining integration or release steps.",
                    run.github_repository, run.issue_number
                );
                let result = self
                    .create_task(
                        user.parse()?,
                        guild.parse()?,
                        &format!("issue:{}", run.id),
                        &repo.name,
                        &task,
                    )
                    .await;
                if let Err(error) = result {
                    // Submission may have reached implementing before an uncertain RPC.
                    sqlx::query("UPDATE issue_runs SET state='blocked',last_error=$3,updated_at=now() WHERE id=$1 AND claim_id=$2 AND state IN ('preparing','implementing')")
                        .bind(run.id).bind(run.claim_id).bind(format!("Dispatch needs review: {error:#}"))
                        .execute(&self.store.pool).await?;
                }
            }
        }
    }
}
